//! 有栈协程：自研上下文切换，零第三方依赖。
//!
//! 只解决一件事：让一段**普通同步代码**在中途让出 CPU，之后再从原处继续。
//! 用户签名不变，没有 `async`、没有 `Send + 'static`、可以跨让出点借用 `&mut`——
//! 这些约束的根源是"状态机由编译器生成、必须跨 await 存活"，有栈协程没有这个前提。
//!
//! 切换语义（x86_64 SysV）：
//!   rsw_save_switch(from: *mut *mut u8, to: *const u8)
//!   保存当前 rsp 到 *from，跳到 to 指向的栈继续跑。
//!   保存/恢复 callee-saved：rbp rbx r12 r13 r14 r15。

use std::alloc::{alloc, dealloc, Layout};
use std::arch::global_asm;
use std::cell::{Cell, RefCell};
use std::time::Instant;

/// 单条协程栈。固定大小便于验证；生产要按 Go 的做法做按需增长。
pub const STACK_SIZE: usize = 64 * 1024;
/// 每个 worker 线程缓存的空闲栈上限。协程数 ≈ 同时在跑的请求数，
/// 峰值过去后保留一部分复用，避免每次 `alloc/dealloc` 都打到全局分配器。
const POOL_MAX: usize = 256;

#[cfg(target_arch = "x86_64")]
global_asm!(
    r#"
.globl rsw_save_switch
rsw_save_switch:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov rsp, rsi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

.globl rsw_entry_stub
rsw_entry_stub:
    mov rdi, r12
    call rsw_on_entry
    ud2
"#
);

extern "C" {
    fn rsw_save_switch(from: *mut *mut u8, to: *const u8);
    fn rsw_entry_stub();
}

/// 协程状态。`Blocked` 表示它已经把重入队交给别人（定时器），worker 不该再碰它。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Runnable,
    Blocked,
    Done,
}

struct StackSlot {
    ptr: *mut u8,
    layout: Layout,
}

thread_local! {
    static POOL: RefCell<Vec<StackSlot>> = const { RefCell::new(Vec::new()) };
}

fn take_stack() -> StackSlot {
    if let Some(s) = POOL.with(|p| p.borrow_mut().pop()) {
        return s;
    }
    let layout = Layout::from_size_align(STACK_SIZE, 16).expect("栈布局");
    let ptr = unsafe { alloc(layout) };
    assert!(!ptr.is_null(), "协程栈分配失败");
    StackSlot { ptr, layout }
}

fn give_stack(s: StackSlot) {
    let (ptr, layout) = (s.ptr, s.layout);
    let kept = POOL.with(|p| {
        let mut p = p.borrow_mut();
        if p.len() < POOL_MAX {
            p.push(StackSlot { ptr, layout });
            true
        } else {
            false
        }
    });
    if !kept {
        unsafe { dealloc(ptr, layout) };
    }
}

/// 一条协程。
#[repr(C)]
pub struct Task {
    /// 保存的栈指针。切换只认它。
    pub sp: Cell<*mut u8>,
    stack: Cell<Option<StackSlot>>,
    pub state: Cell<TaskState>,
    entry: Cell<Option<Box<dyn FnOnce()>>>,
    /// `sleep` 的到期时刻，由 worker 在协程完整让出后读取并登记定时器
    pub deadline: Cell<Option<Instant>>,
    /// 归属 worker。协程**绝不允许跨线程**：它可能正借用着 `!Send` 的 `Context`。
    pub owner: usize,
}

/// `*mut Task` 的裸载体。协程本身不跨线程，跨线程传递的只有"唤醒"这件事，
/// 而唤醒走的是带锁队列，因此可以断言 Send + Sync。
#[derive(Clone, Copy)]
pub struct TaskPtr(pub *mut Task);
unsafe impl Send for TaskPtr {}
unsafe impl Sync for TaskPtr {}

impl TaskPtr {
    pub unsafe fn get<'a>(self) -> &'a Task {
        unsafe { &*self.0 }
    }
}

impl Task {
    pub fn new(f: impl FnOnce() + 'static, owner: usize) -> *mut Task {
        let slot = take_stack();
        let task = Box::into_raw(Box::new(Task {
            sp: Cell::new(std::ptr::null_mut()),
            stack: Cell::new(Some(slot)),
            state: Cell::new(TaskState::Runnable),
            entry: Cell::new(Some(Box::new(f))),
            deadline: Cell::new(None),
            owner,
        }));

        let slot_ptr = unsafe { (*task).stack.take().expect("栈已就位") };
        unsafe {
            let hi = slot_ptr.ptr.add(STACK_SIZE) as usize;
            let mut p = hi;
            p -= 8;
            *(p as *mut usize) = rsw_entry_stub as *const () as usize;
            p -= 8;
            *(p as *mut usize) = 0; // rbp
            p -= 8;
            *(p as *mut usize) = 0; // rbx
            p -= 8;
            *(p as *mut usize) = task as usize; // r12 = task
            p -= 8;
            *(p as *mut usize) = 0; // r13
            p -= 8;
            *(p as *mut usize) = 0; // r14
            p -= 8;
            *(p as *mut usize) = 0; // r15
            (*task).sp.set(p as *mut u8);
            (*task).stack.set(Some(slot_ptr));
        }
        task
    }

    /// # Safety
    /// `from` 必须是本线程上一个稳定存在的 `*mut u8` 槽位（切换会把当前 rsp 写进去）。
    pub unsafe fn resume(&self, from: *mut *mut u8) {
        unsafe { rsw_save_switch(from, self.sp.get()) };
    }

    /// 在协程内调用：保存现场、切到 `to`。
    ///
    /// 新 rsp 必须**由汇编直接写进 `self.sp`**。若先落到局部变量、等切回来再回填，
    /// worker 下次 resume 拿到的仍是旧值（初始栈顶），协程会永远从入口重跑。
    /// # Safety
    /// `to` 必须是某个已暂停协程（或本 worker）保存的合法栈指针。
    pub unsafe fn suspend(&self, to: *const u8) {
        unsafe { rsw_save_switch(self.sp.as_ptr(), to) };
    }

    pub fn is_done(&self) -> bool {
        self.state.get() == TaskState::Done
    }
}

/// 回收协程：栈回池子。
///
/// # Safety
/// 只在 owner worker 里、确认 `Done` 之后调用；调用后指针立即失效。
pub unsafe fn destroy(task: TaskPtr) {
    let t = unsafe { Box::from_raw(task.0) };
    if let Some(slot) = t.stack.take() {
        give_stack(slot);
    }
}

/// 协程入口：由 `rsw_entry_stub` 调用，**永不返回**。
/// # Safety
/// 只允许由 `rsw_entry_stub` 以正确的栈现场调用，`task` 必须是活着的协程指针。
#[no_mangle]
pub unsafe extern "C" fn rsw_on_entry(task: *mut Task) {
    let t = unsafe { &*task };
    if let Some(f) = t.entry.take() {
        // panic 必须在协程栈内被吃掉：我们的帧是伪造的，
        // 一旦展开跨出这里，unwind 会沿错误的 rbp 链走下去直接炸掉进程。
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    }
    t.state.set(TaskState::Done);
    crate::server::mn::sched::finish_current_and_park()
}
