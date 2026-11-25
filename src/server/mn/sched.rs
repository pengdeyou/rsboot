//! 每 worker 一个调度器：本地队列 + 定时器回投 + 唤醒器联动。
//!
//! ## 为什么协程绝不跨线程
//!
//! 协程可能正借用着 `Context`（内含 `RefCell<Session>`，`!Send`）。把它搬到
//! 另一个 worker 上恢复就是 UB。所以这里只有**唤醒消息**跨线程（经由带锁队列），
//! 协程本体永远在它出生的那条 worker 线程上跑。
//!
//! 这也是与 tokio 的根本差异：tokio 的任务必须 `Send`，因为它真的会在任意 worker
//! 上恢复；我们不需要那条约束，代价是放弃"任务级负载均衡"——换成连接在 worker
//! 之间借调（见 `server::mod` 的 rebalance）。
//!
//! ## 为什么运行时不是全局的
//!
//! 早先版本把"所有 worker 的队列"放在进程级 `static`，同一进程起第二个 Server
//! （测试并行、灰度双实例）时，两个实例的 worker id 会撞车，定时器把协程投进
//! 别人的队列——请求挂死不返回（实测表现为偶发 10s 超时）。所以运行时必须由
//! `Server` 持有、随实例走。

use std::cmp::Ordering as CmpOrd;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::co::{destroy, Task, TaskPtr, TaskState};
use crate::server::poll::Waker;

/// 一个 worker 的跨线程入口：别人往这里投"该醒了"。
pub struct Shared {
    queue: Mutex<VecDeque<TaskPtr>>,
    waker: Waker,
}

impl Shared {
    pub fn new(waker: Waker) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
            waker,
        })
    }

    fn push(&self, t: TaskPtr) {
        self.queue.lock().unwrap().push_back(t);
        self.waker.notify();
    }

    fn pop(&self) -> Option<TaskPtr> {
        self.queue.lock().unwrap().pop_front()
    }

    fn len(&self) -> usize {
        self.queue.lock().unwrap().len()
    }
}

struct Entry {
    deadline: Instant,
    task: TaskPtr,
}

impl PartialEq for Entry {
    fn eq(&self, o: &Self) -> bool {
        self.deadline == o.deadline
    }
}
impl Eq for Entry {}
// BinaryHeap 是最大堆，反过来比让最早到期的浮到顶
impl Ord for Entry {
    fn cmp(&self, o: &Self) -> CmpOrd {
        o.deadline.cmp(&self.deadline)
    }
}
impl PartialOrd for Entry {
    fn partial_cmp(&self, o: &Self) -> Option<CmpOrd> {
        Some(self.cmp(o))
    }
}

/// 一次服务运行期的调度器状态。由 `Server` 持有，绝不放在进程级 `static`。
pub struct Runtime {
    workers: Mutex<Vec<Arc<Shared>>>,
    timer: Mutex<BinaryHeap<Entry>>,
    cv: Condvar,
    timer_started: AtomicBool,
}

impl Runtime {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            workers: Mutex::new(Vec::new()),
            timer: Mutex::new(BinaryHeap::new()),
            cv: Condvar::new(),
            timer_started: AtomicBool::new(false),
        })
    }

    /// worker 注册后就位。`id` 必须与注册顺序一致。
    pub fn register(&self, id: usize, shared: Arc<Shared>) {
        let mut w = self.workers.lock().unwrap();
        if w.len() <= id {
            w.resize_with(id + 1, || Arc::clone(&shared));
        }
        w[id] = shared;
    }

    fn push_to(&self, owner: usize, t: TaskPtr) {
        let w = self.workers.lock().unwrap();
        if let Some(s) = w.get(owner) {
            unsafe { t.get().state.set(TaskState::Runnable) };
            s.push(t);
        }
    }

    fn schedule(&self, at: Instant, t: TaskPtr) {
        self.timer
            .lock()
            .unwrap()
            .push(Entry { deadline: at, task: t });
        self.cv.notify_one();
    }

    /// 启定时器线程（幂等）。每个 `Runtime` 一个。
    pub fn start_timer(self: &Arc<Self>) {
        if self.timer_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = Arc::clone(self);
        std::thread::Builder::new()
            .name("rsboot-timer".into())
            .spawn(move || me.timer_loop())
            .expect("定时器线程");
    }

    fn timer_loop(self: Arc<Self>) {
        loop {
            let now = Instant::now();
            let mut due: Vec<TaskPtr> = Vec::new();
            let wait = {
                let mut h = self.timer.lock().unwrap();
                while let Some(e) = h.peek() {
                    if e.deadline <= now {
                        let t = e.task;
                        h.pop();
                        due.push(t);
                    } else {
                        break;
                    }
                }
                match h.peek() {
                    Some(e) => e.deadline.saturating_duration_since(now),
                    None => Duration::from_millis(50),
                }
            };
            if due.is_empty() {
                let _ = self.cv.wait_timeout(self.timer.lock().unwrap(), wait);
                continue;
            }
            for t in due {
                let owner = unsafe { t.get().owner };
                self.push_to(owner, t);
            }
        }
    }
}

struct Local {
    id: usize,
    shared: Arc<Shared>,
    runtime: Arc<Runtime>,
    /// 切回 worker 时保存的栈指针。用 `Cell` 是为了拿到稳定的 `*mut *mut u8`
    /// 交给汇编直接写——经由 `RefCell` 的 `&mut` 逃逸成裸指针会踩借用规则。
    sp: std::cell::Cell<*mut u8>,
    current: TaskPtr,
    /// 正在等 fd 就绪的协程。worker 每轮会把它们的 fd 拼进 `poll`，
    /// 就绪时把协程放回队列——这是"等 socket 也能让出"的全部机制。
    waits: Vec<(std::os::raw::c_int, TaskPtr)>,
}

thread_local! {
    static LOCAL: std::cell::RefCell<Option<Local>> = const { std::cell::RefCell::new(None) };
}

/// worker 线程启动时调用一次。之后 `spawn/sleep/yield_now` 才有意义。
pub fn init(runtime: Arc<Runtime>, id: usize, shared: Arc<Shared>) {
    runtime.register(id, Arc::clone(&shared));
    LOCAL.with(|l| {
        *l.borrow_mut() = Some(Local {
            id,
            shared,
            runtime,
            sp: std::cell::Cell::new(std::ptr::null_mut()),
            current: TaskPtr(std::ptr::null_mut()),
            waits: Vec::new(),
        });
    });
}

/// 当前线程能不能让出（即处在 worker 的调度器里）。
pub fn ready() -> bool {
    LOCAL.with(|l| l.borrow().is_some())
}

fn with_local<R>(f: impl FnOnce(&mut Local) -> R) -> Option<R> {
    LOCAL.with(|l| l.borrow_mut().as_mut().map(f))
}

/// 在 worker 里创建协程并入队。返回 false 表示当前线程没有调度器。
pub fn spawn(f: impl FnOnce() + 'static) -> bool {
    let Some((shared, id)) = with_local(|l| (Arc::clone(&l.shared), l.id)) else {
        return false;
    };
    let t = Task::new(f, id);
    shared.push(TaskPtr(t));
    true
}

/// worker 主循环里调用：把本地队列跑一轮，最多 `limit` 条。
pub fn pump(limit: usize) -> usize {
    let mut done = 0usize;
    for _ in 0..limit {
        let Some(t) = with_local(|l| l.shared.pop()).flatten() else {
            break;
        };
        let Some(sp_slot) = with_local(|l| l.sp.as_ptr()) else {
            break;
        };
        with_local(|l| l.current = t);
        unsafe { t.get().resume(sp_slot) };

        let task = unsafe { t.get() };
        match task.state.get() {
            TaskState::Done => unsafe { destroy(t) },
            // 协程已完整让出，这时登记定时器不会和它抢跑
            TaskState::Blocked => {
                if let Some(dl) = task.deadline.get() {
                    if let Some(rt) = with_local(|l| Arc::clone(&l.runtime)) {
                        rt.schedule(dl, t);
                    }
                }
            }
            TaskState::Runnable => {}
        }
        with_local(|l| l.current = TaskPtr(std::ptr::null_mut()));
        done += 1;
    }
    done
}

/// 协程里调用：等 `fd` 可读/可写就绪，期间让出 worker。
///
/// 调用前应先用 0 超时的 `poll` 检查一次——已经就绪就别折腾调度器。
pub fn wait_fd(fd: std::os::raw::c_int) {
    let t = current_task();
    if t.0.is_null() {
        return;
    }
    with_local(|l| l.waits.push((fd, t)));
    unsafe {
        // deadline 留空：worker 看到 Blocked 但没 deadline 就不会去排定时器
        t.get().deadline.set(None);
        t.get().state.set(TaskState::Blocked);
        t.get().suspend(my_sp());
    }
}

/// worker 拼 `poll` 用的等待 fd 列表。
pub fn wait_fds() -> Vec<std::os::raw::c_int> {
    with_local(|l| l.waits.iter().map(|(fd, _)| *fd).collect()).unwrap_or_default()
}

/// 某个等待项就绪：把协程放回队列并移出等待表。
///
/// **调用方必须按下标降序处理**：`swap_remove` 会把尾部搬到当前位置，
/// 升序会让后面的下标指向别人（这正是 S9 缺陷 D 的同款问题）。
pub fn wake_wait(i: usize) {
    let picked: Option<(Arc<Shared>, TaskPtr)> = with_local(|l| {
        if i < l.waits.len() {
            let (_, t) = l.waits.swap_remove(i);
            Some((Arc::clone(&l.shared), t))
        } else {
            None
        }
    })
    .flatten();
    let Some((shared, t)) = picked else {
        return;
    };
    unsafe { t.get().state.set(TaskState::Runnable) };
    shared.queue.lock().unwrap().push_back(t);
}

/// 队列里还有多少待跑。
pub fn pending() -> usize {
    with_local(|l| l.shared.len()).unwrap_or(0)
}

fn current_task() -> TaskPtr {
    with_local(|l| l.current).unwrap_or(TaskPtr(std::ptr::null_mut()))
}

fn my_sp() -> *const u8 {
    with_local(|l| l.sp.get() as *const u8).unwrap_or(std::ptr::null())
}

/// 让出 CPU，稍后由同一个 worker 重新调度。
pub fn yield_now() {
    let t = current_task();
    if t.0.is_null() {
        return;
    }
    let (shared, to) = match with_local(|l| (Arc::clone(&l.shared), l.sp.get() as *const u8)) {
        Some(v) => v,
        None => return,
    };
    unsafe { t.get().state.set(TaskState::Runnable) };
    shared.queue.lock().unwrap().push_back(t);
    unsafe { t.get().suspend(to) };
}

/// 睡 `d`，期间不占用 worker。
pub fn sleep(d: Duration) {
    let t = current_task();
    if t.0.is_null() {
        std::thread::sleep(d);
        return;
    }
    unsafe {
        t.get().deadline.set(Some(Instant::now() + d));
        t.get().state.set(TaskState::Blocked);
        // 登记动作交给 worker 在 `resume` 返回后做：那时协程已经彻底挂起，
        // 不存在"定时器先把它唤醒、而它还没切走"的抢跑窗口（踩过，栈被打烂）。
        t.get().suspend(my_sp());
    }
}

/// 协程主体跑完，交还控制权，永不返回。
pub fn finish_current_and_park() -> ! {
    let t = current_task();
    unsafe { t.get().suspend(my_sp()) };
    unreachable!("已结束的协程不应被重新调度")
}
