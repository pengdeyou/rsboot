//! 待抓队列（frontier）：去重 + 深度限制 + per-host 限速。
//!
//! 设计照 `spider` 的 frontier 和 `spider-lib` 的 Scheduler，
//! 但落到同步模型：`pop` 是一个普通的 `&mut self` 调用，多个工作线程
//! 抢同一把 `Mutex<Frontier>`，不需要 async runtime。
//!
//! 为什么按 host 分桶而不是一条 FIFO：
//! 礼貌抓取要按**站**限速。一条 FIFO 遇到"某站 crawl-delay 2 秒"时，
//! 队头卡住全场都得等；分桶后可以跳过还没到点的站去抓别的站，
//! 只有在**所有**站都没到点时才真睡。

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::url::Url;

/// 队列里的一个待抓任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub url: Url,
    /// 距种子几跳。深度限制靠它，不做"路径深度"猜测。
    pub depth: u32,
    /// 从哪个页面发现它
    pub referer: Option<String>,
}

/// `push` 的结果。重复和超深都要能区分出来，统计里是两栏。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    Accepted,
    Duplicate,
    TooDeep,
}

/// `pop` 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pop {
    Ready(Item),
    /// 还轮不到你：要么所有站都在冷却，要么队列空了但还有别的 worker 在路上。
    /// `Ready` 之后必须 `release`，否则这条会一直算"在路上"。
    Wait(Duration),
    /// 真的空了：队列没活，也没有任何 worker 在飞。这时候退出是安全的。
    Empty,
}

struct Bucket {
    items: VecDeque<Item>,
    /// 这个站下一次允许抓的时间
    next_at: Instant,
    /// 这个站自己的限速（robots.txt 的 `Crawl-delay`），没设就用全局
    delay: Option<Duration>,
    /// 这个站正在被抓、还没归还的条数
    inflight: usize,
}

/// 断点日志（WAL）。
///
/// 只记两种事件：`+ <depth> <key>`（入队）和 `- <key>`（抓完）。恢复时
/// `seen` 取所有 `+`，`pending` 取"有 `+` 没有 `-`"的那些，按原顺序重放。
///
/// 为什么记两行而不是只记去重集合：只记 `seen` 的话，中断时队列里
/// 那些"已入队但没抓"的 URL 就丢了——恢复后从种子重新开始，如果种子在
/// `seen` 里，队列直接空掉，等于白爬。
struct Journal {
    file: File,
}

/// 回放结果：`(seen, pending)`。pending 保持日志里的入队顺序。
type Replayed = (HashSet<String>, Vec<(u32, String)>);

impl Journal {
    fn open_append(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            file: OpenOptions::new().create(true).append(true).open(path)?,
        })
    }

    fn load(path: &Path) -> std::io::Result<Replayed> {
        let mut seen = HashSet::new();
        let mut pending: Vec<(u32, String)> = Vec::new();
        match File::open(path) {
            Ok(f) => {
                for line in BufReader::new(f).lines() {
                    let line = match line {
                        Ok(l) => l,
                        Err(_) => continue, // 中断时最后一行可能是半截的
                    };
                    let mut it = line.splitn(3, ' ');
                    match (it.next(), it.next(), it.next()) {
                        (Some("+"), Some(depth), Some(key)) => {
                            if let Ok(d) = depth.parse::<u32>() {
                                if seen.insert(key.to_string()) {
                                    pending.push((d, key.to_string()));
                                }
                            }
                        }
                        (Some("-"), Some(key), None) => {
                            pending.retain(|(_, k)| k != key);
                        }
                        _ => {}
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok((seen, pending))
    }

    fn note_pushed(&mut self, depth: u32, key: &str) {
        let _ = writeln!(self.file, "+ {depth} {key}");
    }

    fn note_done(&mut self, key: &str) {
        let _ = writeln!(self.file, "- {key}");
    }
}

/// 见 `module docs`。
pub struct Frontier {
    buckets: HashMap<Box<str>, Bucket>,
    /// 有活且（曾经）可以抓的站，可能含"还在冷却期"的站——`pop` 会跳过它们
    ready: VecDeque<Box<str>>,
    seen: HashSet<Box<str>>,
    max_depth: u32,
    delay: Option<Duration>,
    len: usize,
    dup: usize,
    too_deep: usize,
    /// 每个站同时最多几个在飞；0 = 不限
    max_per_host: usize,
    /// 全部在飞的总数。用来区分"队列真空了"和"队列空但那几发还在路上"
    inflight_total: usize,
    journal: Option<Journal>,
}

impl Frontier {
    pub fn new(max_depth: u32, delay: Option<Duration>) -> Self {
        Self {
            buckets: HashMap::new(),
            ready: VecDeque::new(),
            seen: HashSet::new(),
            max_depth,
            delay,
            len: 0,
            dup: 0,
            too_deep: 0,
            max_per_host: 0,
            inflight_total: 0,
            journal: None,
        }
    }

    /// 从断点日志恢复。返回 `(队列, 恢复出的待抓条数)`。
    ///
    /// 日志不存在等于从头开始；存在则 `seen` 与 `pending` 都按日志重建，
    /// 之后的行为和"没中断过"一致（继续写日志、继续去重）。
    pub fn resume(
        path: impl AsRef<Path>,
        max_depth: u32,
        delay: Option<Duration>,
    ) -> std::io::Result<(Self, usize)> {
        let path = path.as_ref();
        let (seen, pending) = Journal::load(path)?;
        let n = pending.len();
        let now = Instant::now();
        let mut f = Self {
            seen: seen.into_iter().map(|k| k.into_boxed_str()).collect(),
            journal: Some(Journal::open_append(path)?),
            ..Self::new(max_depth, delay)
        };
        for (depth, key) in pending {
            if let Some(url) = Url::parse(&key) {
                f.push_restored(
                    Item {
                        url,
                        depth,
                        referer: Some("checkpoint".to_string()),
                    },
                    now,
                );
            }
        }
        Ok((f, n))
    }

    /// 恢复专用：跳过 `seen` 检查（`seen` 已经包含它了）。
    fn push_restored(&mut self, item: Item, now: Instant) {
        let host: Box<str> = item.url.host_header().into_boxed_str();
        let b = self.buckets.entry(host.clone()).or_insert_with(|| Bucket {
            items: VecDeque::new(),
            next_at: now,
            delay: None,
            inflight: 0,
        });
        b.items.push_back(item);
        self.len += 1;
        if !self.ready.contains(&host) {
            self.ready.push_back(host);
        }
    }

    /// 入队。已见过 / 太深 都会被拒，且都记账。
    pub fn push(&mut self, item: Item, now: Instant) -> Push {
        if item.depth > self.max_depth {
            self.too_deep += 1;
            return Push::TooDeep;
        }
        let key_str = item.url.key();
        let key: Box<str> = key_str.clone().into_boxed_str();
        if !self.seen.insert(key) {
            self.dup += 1;
            return Push::Duplicate;
        }
        let host: Box<str> = item.url.host_header().into_boxed_str();
        let b = self.buckets.entry(host.clone()).or_insert_with(|| Bucket {
            items: VecDeque::new(),
            next_at: now, // 新站立刻可抓
            delay: None,
            inflight: 0,
        });
        if let Some(j) = &mut self.journal {
            j.note_pushed(item.depth, &key_str);
        }
        b.items.push_back(item);
        self.len += 1;
        // 冷却中的站也排进 ready：`pop` 会检查时间并跳过，靠"只在能抓时重复排"
        // 会漏掉"排进去后才发现没到点"的那一轮。
        if !self.ready.contains(&host) {
            self.ready.push_back(host);
        }
        Push::Accepted
    }

    /// 取一个现在就能抓的任务。
    pub fn pop(&mut self, now: Instant) -> Pop {
        let n = self.ready.len();
        let mut min_wait: Option<Duration> = None;
        for _ in 0..n {
            let host = match self.ready.pop_front() {
                Some(h) => h,
                None => break,
            };
            let bucket = match self.buckets.get_mut(&host) {
                Some(b) => b,
                None => continue,
            };
            if bucket.items.is_empty() {
                continue;
            }
            if bucket.next_at > now {
                let d = bucket.next_at - now;
                min_wait = Some(min_wait.map_or(d, |m| m.min(d)));
                self.ready.push_back(host);
                continue;
            }
            // 这个站已经挤了这么多人，先让别人上；不能 return Empty——队列里还有活，
            // 只是全在飞，worker 该睡一下再来
            if self.max_per_host > 0 && bucket.inflight >= self.max_per_host {
                self.ready.push_back(host);
                continue;
            }
            let item = match bucket.items.pop_front() {
                Some(i) => i,
                None => continue,
            };
            if let Some(j) = &mut self.journal {
                j.note_done(&item.url.key());
            }
            bucket.next_at = now + bucket.delay.or(self.delay).unwrap_or_default();
            bucket.inflight += 1;
            self.inflight_total += 1;
            if !bucket.items.is_empty() {
                self.ready.push_back(host);
            }
            self.len -= 1;
            return Pop::Ready(item);
        }
        if self.len > 0 {
            // 有活但都在冷却：睡到最早那个
            Pop::Wait(min_wait.unwrap_or_else(|| Duration::from_millis(10)))
        } else if self.inflight_total > 0 {
            // 队列空，但还有别的 worker 在路上——它们抓完可能推新链接进来。
            // 这时候报 `Empty` 会让 worker 全部提前退出，把尾部整段丢掉
            Pop::Wait(Duration::from_millis(1))
        } else {
            Pop::Empty
        }
    }

    /// 一个任务抓完了，把在飞名额还回去。不还就等于这个站永久的并发上限 -1。
    ///
    /// 抓取路径里必须有个 Drop 兜底（handler panic 也要还），见 `lib.rs` 的 `Release`。
    pub fn release(&mut self, host: &str) {
        if let Some(b) = self.buckets.get_mut(host) {
            if b.inflight > 0 {
                b.inflight -= 1;
                self.inflight_total -= 1;
            }
            if !b.items.is_empty() {
                let host: Box<str> = host.into();
                if !self.ready.contains(&host) {
                    self.ready.push_back(host);
                }
            }
        }
    }

    /// 每个站同时最多几个在飞。`0` = 不限（默认）。
    ///
    /// 和 `delay` 是两件事：`delay` 管"两次请求隔多久"，这个管"同时几发"。
    /// 只限速不限并发时，4 个 worker 会同时向同一个站在 0 延迟下连开 4 发——
    /// 对人家不算客气，自己这边也全卡在这一个站上。
    pub fn set_max_per_host(&mut self, n: usize) {
        self.max_per_host = n;
    }

    /// 某个站当前在飞的条数（测试用）。
    pub fn in_flight(&self, host: &str) -> usize {
        self.buckets.get(host).map_or(0, |b| b.inflight)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn seen_len(&self) -> usize {
        self.seen.len()
    }

    /// `(去重命中数, 超深丢弃数)`
    pub fn rejected(&self) -> (usize, usize) {
        (self.dup, self.too_deep)
    }

    /// 给某个站设单独的限速（robots.txt 的 `Crawl-delay` 用这个）。
    /// 已经在跑的站会在下一次 `pop` 之后生效。
    pub fn set_host_delay(&mut self, host: &str, delay: Duration) {
        if let Some(b) = self.buckets.get_mut(host) {
            b.delay = Some(b.delay.map_or(delay, |d| d.max(delay)));
            let at = Instant::now() + b.delay.unwrap_or(delay);
            if at > b.next_at {
                b.next_at = at;
            }
        }
    }

    /// 已经见过这个 URL 没有。测试和外部预筛用。
    pub fn has_seen(&self, url: &Url) -> bool {
        self.seen.contains(url.key().as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(u: &str, depth: u32) -> Item {
        Item {
            url: Url::parse(u).unwrap(),
            depth,
            referer: None,
        }
    }

    #[test]
    fn dedups_and_counts() {
        let now = Instant::now();
        let mut f = Frontier::new(2, None);
        assert_eq!(f.push(item("http://a.com/x", 0), now), Push::Accepted);
        assert_eq!(f.push(item("http://a.com/x", 0), now), Push::Duplicate);
        // 等价写法也要被判重
        assert_eq!(f.push(item("http://A.com:80/x", 0), now), Push::Duplicate);
        assert_eq!(f.len(), 1);
        assert_eq!(f.rejected(), (2, 0));
    }

    #[test]
    fn depth_limit() {
        let now = Instant::now();
        let mut f = Frontier::new(1, None);
        assert_eq!(f.push(item("http://a.com/1", 1), now), Push::Accepted);
        assert_eq!(f.push(item("http://a.com/2", 2), now), Push::TooDeep);
        assert_eq!(f.rejected(), (0, 1));
    }

    #[test]
    fn fifo_within_one_host() {
        let now = Instant::now();
        let mut f = Frontier::new(5, None);
        for i in 0..3 {
            f.push(item(&format!("http://a.com/{i}"), 0), now);
        }
        for i in 0..3 {
            match f.pop(now) {
                Pop::Ready(it) => {
                    assert_eq!(it.url.path, format!("/{i}"));
                    f.release("a.com");
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(f.pop(now), Pop::Empty);
    }

    #[test]
    fn slow_host_does_not_block_fast_host() {
        let now = Instant::now();
        let delay = Duration::from_millis(100);
        let mut f = Frontier::new(5, Some(delay));
        f.push(item("http://slow.com/a", 0), now);
        f.push(item("http://slow.com/b", 0), now);
        f.push(item("http://fast.com/c", 0), now);

        let first = f.pop(now);
        assert!(matches!(first, Pop::Ready(_)));
        // 无论第一枪打到谁，第二枪必须能拿到另一个站的（而不是等 100ms）
        let second = f.pop(now);
        match second {
            Pop::Ready(it) => {
                let first_host = match first {
                    Pop::Ready(i) => i.url.host,
                    _ => unreachable!(),
                };
                assert_ne!(it.url.host, first_host, "同一站应当被限速挡住");
            }
            other => panic!("不该阻塞: {other:?}"),
        }
        // 现在只剩 slow 的第二个，应该给出 Wait
        match f.pop(now) {
            Pop::Wait(d) => assert!(d <= delay),
            other => panic!("{other:?}"),
        }
        // 时间到了就能拿到
        match f.pop(now + delay) {
            Pop::Ready(it) => assert_eq!(it.url.host, "slow.com"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn resume_replays_only_unfinished() {
        let dir = std::env::temp_dir().join(format!("claw-fr-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("journal");
        let _ = std::fs::remove_file(&path);
        let now = Instant::now();
        {
            let (mut f, n) = Frontier::resume(&path, 5, None).unwrap();
            assert_eq!(n, 0, "没有日志就是从头开始");
            f.push(item("http://a.com/1", 0), now);
            f.push(item("http://a.com/2", 0), now);
            match f.pop(now) {
                Pop::Ready(it) => assert_eq!(it.url.path, "/1"),
                other => panic!("{other:?}"),
            }
        } // 模拟进程退出（队列丢了）

        let (mut f, n) = Frontier::resume(&path, 5, None).unwrap();
        assert_eq!(n, 1, "只该恢复没抓完的那条");
        assert_eq!(f.len(), 1);
        assert!(
            f.has_seen(&Url::parse("http://a.com/1").unwrap()),
            "抓过的不能在恢复后又抓一遍"
        );
        match f.pop(Instant::now()) {
            Pop::Ready(it) => assert_eq!(it.url.path, "/2"),
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_host_inflight_is_capped_but_not_empty() {
        let now = Instant::now();
        let mut f = Frontier::new(5, None);
        f.set_max_per_host(2);
        for i in 0..3 {
            f.push(item(&format!("http://a.com/{i}"), 0), now);
        }
        for _ in 0..2 {
            assert!(matches!(f.pop(now), Pop::Ready(_)));
        }
        // 这个站满了，但队列里还有一条没抓：必须是 Wait 而不是 Empty，
        // 否则 worker 直接退出，整次抓取就丢了尾巴
        match f.pop(now) {
            Pop::Wait(d) => assert!(d <= Duration::from_millis(50)),
            other => panic!("{other:?}"),
        }
        assert_eq!(f.in_flight("a.com"), 2);

        // 别的站不该被这个站的并发挡住
        f.push(item("http://b.com/x", 0), now);
        match f.pop(now) {
            Pop::Ready(it) => {
                assert_eq!(it.url.host, "b.com");
                f.release("b.com");
            }
            other => panic!("{other:?}"),
        }

        // 还一个名额就能继续
        f.release("a.com");
        assert_eq!(f.in_flight("a.com"), 1);
        match f.pop(now) {
            Pop::Ready(it) => assert_eq!(it.url.host, "a.com"),
            other => panic!("{other:?}"),
        }
        // 多发几次 release 不能下溢
        for _ in 0..5 {
            f.release("a.com");
        }
        assert_eq!(f.in_flight("a.com"), 0);
        assert_eq!(f.pop(now), Pop::Empty);
    }

    /// 队列空了但有一发还在路上：这时候报 `Empty` 会让 idle 的 worker 提前退出，
    /// 那一发抓回来推的新链接就没人接了。
    #[test]
    fn empty_queue_with_inflight_is_not_drained() {
        let now = Instant::now();
        let mut f = Frontier::new(1, None);
        f.push(item("http://a.com/x", 0), now);
        assert!(matches!(f.pop(now), Pop::Ready(_)));
        assert_eq!(f.len(), 0);
        assert!(matches!(f.pop(now), Pop::Wait(_)));
        f.release("a.com");
        assert_eq!(f.pop(now), Pop::Empty);
    }

    #[test]
    fn many_hosts_round_robin() {
        let now = Instant::now();
        let mut f = Frontier::new(5, Some(Duration::from_millis(50)));
        for h in ["a.com", "b.com", "c.com"] {
            for i in 0..2 {
                f.push(item(&format!("http://{h}/{i}"), 0), now);
            }
        }
        let mut hosts = Vec::new();
        for _ in 0..3 {
            match f.pop(now) {
                Pop::Ready(it) => hosts.push(it.url.host),
                other => panic!("{other:?}"),
            }
        }
        hosts.sort();
        assert_eq!(hosts, vec!["a.com", "b.com", "c.com"]);
    }
}
