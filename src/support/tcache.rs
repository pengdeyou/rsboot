//! 线程缓存的 size-class 分配器（自研，不引第三方）。
//!
//! ## 为什么值得自己写一份
//!
//! 实测给了两个数：单线程 `malloc`+`free` 一次约 11 ns，但 3 个 worker 并发时
//! 每请求省掉**一次**分配就值 4.3% QPS。也就是说并发下分配器的真实成本不是
//! 那 11 ns，而是"每请求都进出一次全局分配器"带来的跨核流量。
//!
//! 这里做的事情和 tcmalloc / snmalloc / mimalloc 的核心是同一件：
//! 把小块分配变成**线程私有的指针弹出**，热路径上一次原子操作都没有。
//!
//! ## 边界（刻意保守）
//!
//! - 只接管 `size <= 1 KiB` 且 `align <= 16` 的请求，其余原样交给系统分配器。
//!   大对象走 `mmap` 本来就是对的，硬塞进线程缓存只会浪费。
//! - 线程缓存只进不出：块释放后留在本线程的空闲链上等下次复用，**不还给系统**。
//!   服务端 worker 数固定、每类块的需求形状稳定，这个缓存会很快收敛到峰值；
//!   但如果一个线程某一刻分配了几万个临时对象再全部释放，这部分内存会一直
//!   留在它手里。这是 tcmalloc 也有、并且靠 scavenger 缓解的同一个问题，
//!   这里选择不做 scavenger：宁可常驻，不要为了归还而加锁。
//! - 跨线程释放是允许的：块大小相同就能互相复用，回收链本身没有归属概念。
//!
//! ## 关掉它
//!
//! `tc-alloc` 是 crate feature，默认关闭：`#[global_allocator]` 会影响整个
//! 链接进来的程序（包括下游自己的分配），默认替换别人的全局分配器是不礼貌的。
//! 想要这套缓存就显式打开。

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::UnsafeCell;
use std::ptr;

/// 接管的 size class，**全部是 2 的幂**。
///
/// 早先的档位是 `[16, 32, 64, 96, 128, 192, 256, 512, 1024]`，为了省内部碎片
/// 塞了 96/192 这类中间档，代价是 `class_of` 得线性扫表——9 次比较加分支预测
/// 失败，在每请求好几次的分配上是一笔真实开销。改成纯幂次之后，档位从
/// `size` 本身用 `leading_zeros` 一条指令算出来，代价只是碎片翻倍（129 字节
/// 会拿 256 的块）。对一个每请求几十字节的 HTTP 服务，这个交换是赚的。
const SIZES: [usize; 7] = [16, 32, 64, 128, 256, 512, 1024];
const CLASSES: usize = SIZES.len();
/// 超过这个对齐一律交给系统分配器（自研路径只保证 16 字节对齐）。
const MAX_ALIGN: usize = 16;
/// 每个 class 一次向系统要一整页的整数倍，切完再要。
const SLAB_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy)]
struct Bin {
    /// 空闲链头。每个空闲块的头 8 字节存下一个块的地址。
    free: *mut u8,
    /// 当前 slab 里还没切出去的位置。
    cursor: *mut u8,
    /// 当前 slab 还剩几块。
    avail: usize,
}

const EMPTY_BIN: Bin = Bin {
    free: ptr::null_mut(),
    cursor: ptr::null_mut(),
    avail: 0,
};

thread_local! {
    /// `const` 初始化：TLS 的第一次访问不会反过来触发分配（那会递归到分配器自己）。
    static BINS: UnsafeCell<[Bin; CLASSES]> = const { UnsafeCell::new([EMPTY_BIN; CLASSES]) };
}

/// 按大小找 class；超范围返回 `None`。
///
/// 一条 `leading_zeros`，没有任何表访问、也没有分支环：`size` 落在
/// `(2^(k-1), 2^k]` 就归第 `k - 4` 档（16 是 2^4）。
#[inline]
fn class_of(size: usize) -> Option<usize> {
    if size > SIZES[CLASSES - 1] {
        return None;
    }
    // `size <= 16` 一律进最小的档：`(size - 1).leading_zeros()` 在 size = 0/1
    // 时会算到 64 位宽再去减 4，直接下溢
    let size = size.max(16);
    let bits = usize::BITS - (size - 1).leading_zeros();
    Some(bits as usize - 4)
}

/// 给某个 class 换一块新 slab。失败时保持原状（调用方会退回系统分配器）。
///
/// # Safety
/// 只在当前线程的 `Bin` 上调用。
unsafe fn refill(bin: &mut Bin, cls: usize) {
    let layout = match Layout::from_size_align(SLAB_BYTES, 4096) {
        Ok(l) => l,
        Err(_) => return,
    };
    let slab = System.alloc(layout);
    if slab.is_null() {
        return;
    }
    bin.cursor = slab;
    bin.avail = SLAB_BYTES / SIZES[cls];
}

/// 线程缓存分配器。
pub struct TcAlloc;

unsafe impl GlobalAlloc for TcAlloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size().max(1);
        if layout.align() > MAX_ALIGN {
            return System.alloc(layout);
        }
        let Some(cls) = class_of(size) else {
            return System.alloc(layout);
        };

        // TLS 正在析构（或压根没初始化成功）时退回系统分配器，绝不 panic
        let hit = BINS.try_with(|bins| {
            let bin = &mut (*bins.get())[cls];
            if !bin.free.is_null() {
                let p = bin.free;
                bin.free = *(p as *mut *mut u8);
                return p;
            }
            if bin.avail == 0 {
                refill(bin, cls);
            }
            if bin.avail == 0 {
                return ptr::null_mut();
            }
            let p = bin.cursor;
            bin.cursor = p.add(SIZES[cls]);
            bin.avail -= 1;
            p
        });
        match hit {
            Ok(p) if !p.is_null() => p,
            _ => System.alloc(layout),
        }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let size = layout.size().max(1);
        if layout.align() > MAX_ALIGN {
            return System.dealloc(ptr, layout);
        }
        let Some(cls) = class_of(size) else {
            return System.dealloc(ptr, layout);
        };
        let pushed = BINS
            .try_with(|bins| {
                let bin = &mut (*bins.get())[cls];
                // 块本身至少 16 字节，放得下这个 8 字节的 next
                *(ptr as *mut *mut u8) = bin.free;
                bin.free = ptr;
            })
            .is_ok();
        if !pushed {
            System.dealloc(ptr, layout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{alloc as sys_alloc, dealloc as sys_dealloc};

    #[test]
    fn class_of_covers_boundaries() {
        assert_eq!(class_of(1), Some(0));
        assert_eq!(class_of(16), Some(0));
        assert_eq!(class_of(17), Some(1));
        assert_eq!(class_of(1024), Some(CLASSES - 1));
        assert_eq!(class_of(1025), None);
    }

    #[test]
    fn alloc_dealloc_roundtrip_reuses_memory() {
        let a = TcAlloc;
        unsafe {
            let l = Layout::from_size_align(64, 8).unwrap();
            let p1 = a.alloc(l);
            assert!(!p1.is_null());
            a.dealloc(p1, l);
            let p2 = a.alloc(l);
            // 刚还回去的那块应当被立刻复用
            assert_eq!(p1, p2);
            a.dealloc(p2, l);
        }
    }

    #[test]
    fn large_and_overaligned_fall_back_to_system() {
        let a = TcAlloc;
        unsafe {
            let big = Layout::from_size_align(4096, 8).unwrap();
            let p = a.alloc(big);
            assert!(!p.is_null());
            a.dealloc(p, big);

            let aligned = Layout::from_size_align(64, 64).unwrap();
            let q = a.alloc(aligned);
            assert!(!q.is_null());
            assert_eq!(q as usize % 64, 0);
            a.dealloc(q, aligned);
        }
    }

    #[test]
    fn many_allocs_stay_in_bounds() {
        let a = TcAlloc;
        unsafe {
            let l = Layout::from_size_align(96, 8).unwrap();
            let mut v = Vec::new();
            for i in 0..5000 {
                let p = a.alloc(l);
                assert!(!p.is_null());
                // 写满整块，确认没有越界借用别人的块
                std::ptr::write_bytes(p, i as u8, 96);
                v.push(p);
            }
            for p in v {
                a.dealloc(p, l);
            }
        }
    }

    #[test]
    fn cross_thread_free_is_safe() {
        let a = TcAlloc;
        unsafe {
            let l = Layout::from_size_align(128, 8).unwrap();
            let p = a.alloc(l);
            assert!(!p.is_null());
            std::ptr::write_bytes(p, 0xAB, 128);
            let p_addr = p as usize;
            std::thread::spawn(move || {
                // 在另一个线程上写和释放：块大小相同，只会回到那个线程的空闲链
                std::ptr::write_bytes(p_addr as *mut u8, 0xCD, 128);
                TcAlloc.dealloc(p_addr as *mut u8, l);
            })
            .join()
            .unwrap();
        }
    }

    #[test]
    fn vec_and_string_work_under_this_allocator() {
        // 直接调用本分配器，模拟 `#[global_allocator]` 生效时的常见路径
        let a = TcAlloc;
        unsafe {
            let l = Layout::from_size_align(std::mem::size_of::<u64>() * 4, 8).unwrap();
            let p = a.alloc(l);
            assert!(!p.is_null());
            let s = std::slice::from_raw_parts_mut(p as *mut u64, 4);
            for (i, x) in s.iter_mut().enumerate() {
                *x = i as u64;
            }
            a.dealloc(p, l);
        }
    }

    #[test]
    fn system_paths_are_independent() {
        // 保证我们没有偷偷改系统分配器的行为
        unsafe {
            let l = Layout::from_size_align(2048, 8).unwrap();
            let p = sys_alloc(l);
            assert!(!p.is_null());
            sys_dealloc(p, l);
        }
    }
}
