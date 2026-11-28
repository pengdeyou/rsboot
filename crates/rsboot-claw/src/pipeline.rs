//! 结果旁路（Scrapy 里叫 Pipeline）。
//!
//! handler 负责"下一步抓什么"，sink 负责"抓到的怎么落地"——两件事分开，
//! 是因为它们生命周期不同：handler 是纯函数式的决策，sink 往往带着文件句柄、
//! 数据库连接这类有状态的东西，而且多个 worker 要共享同一个。
//!
//! 自带一个 [`JsonlSink`]：一行一页 JSON，`jq` / DuckDB / 任何日志系统都能直接吃。
//! 这是零依赖能做到的最小可用落盘格式（CSV 会被正文里的逗号和换行毁掉，
//! 自定义分隔符又没人认）。

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::url::Url;
use crate::Page;

/// 抓取结果的接收端。实现必须是线程安全的——所有 worker 共用一份。
pub trait Sink: Send + Sync {
    fn on_page(&self, page: &Page);

    /// 抓取失败。默认什么都不做：多数场景只需要成功的页。
    fn on_error(&self, url: &Url, error: &str) {
        let _ = (url, error);
    }
}

/// 追加写 JSON Lines（一行一条，`\n` 结尾）。
///
/// 为什么不是"攒一批再写"：爬虫的瓶颈在网络，写盘那点开销不算什么；
/// 而攒批意味着进程被杀时会丢最后一批——爬了几个小时的 seen 集合
/// 和一页页结果，能少丢就少丢。
pub struct JsonlSink {
    file: Mutex<File>,
    rows: AtomicUsize,
    failed: AtomicUsize,
}

impl JsonlSink {
    /// 新建（覆盖已有文件）。父目录不会自动创建——路径写错要立刻报错，
    /// 而不是在别处偷偷建出一棵目录树。
    pub fn create(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::from_file(File::create(path)?)
    }

    /// 追加到已有文件（续爬的场景）。
    pub fn append(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::from_file(std::fs::OpenOptions::new().create(true).append(true).open(path)?)
    }

    fn from_file(f: File) -> std::io::Result<Self> {
        Ok(Self {
            file: Mutex::new(f),
            rows: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
        })
    }

    /// 已写成功的行数。
    pub fn rows(&self) -> usize {
        self.rows.load(Ordering::Relaxed)
    }

    /// 写失败的行数（磁盘满之类）。不 panic：爬虫不该因为写日志失败就停。
    pub fn failed(&self) -> usize {
        self.failed.load(Ordering::Relaxed)
    }

    fn write_line(&self, line: &str) {
        let mut f = self.file.lock().unwrap();
        if f.write_all(line.as_bytes()).is_ok() && f.write_all(b"\n").is_ok() {
            self.rows.fetch_add(1, Ordering::Relaxed);
        } else {
            self.failed.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Sink for JsonlSink {
    fn on_page(&self, page: &Page) {
        self.write_line(&page.to_json());
    }

    fn on_error(&self, url: &Url, error: &str) {
        self.write_line(&format!(
            "{{\"url\":{},\"error\":{}}}",
            crate::json_string(&url.to_string()),
            crate::json_string(error)
        ));
    }
}

/// 收在内存里。测试和"跑完再一起处理"的用法。
#[derive(Default)]
pub struct CollectSink {
    pages: Mutex<Vec<Page>>,
    errors: Mutex<Vec<(String, String)>>,
}

impl CollectSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pages(&self) -> Vec<Page> {
        self.pages.lock().unwrap().clone()
    }

    pub fn errors(&self) -> Vec<(String, String)> {
        self.errors.lock().unwrap().clone()
    }
}

impl Sink for CollectSink {
    fn on_page(&self, page: &Page) {
        self.pages.lock().unwrap().push(page.clone());
    }

    fn on_error(&self, url: &Url, error: &str) {
        self.errors
            .lock()
            .unwrap()
            .push((url.to_string(), error.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html::Document;

    fn page(url: &str, title: &str, text: &str) -> Page {
        Page {
            url: Url::parse(url).unwrap(),
            status: 200,
            depth: 1,
            title: title.to_string(),
            description: "d\"esc".to_string(),
            canonical: None,
            doc: Document {
                text: text.to_string(),
                ..Default::default()
            },
            body_len: 10,
            elapsed: std::time::Duration::from_millis(3),
        }
    }

    #[test]
    fn json_escapes_quotes_and_newlines() {
        let p = page("http://a.com/x?q=1", "标\"题", "第一行\n第二行\t制表");
        let j = p.to_json();
        assert!(j.starts_with("{\"url\":\"http://a.com/x?q=1\""), "{j}");
        assert!(j.contains("标\\\"题"), "{j}");
        assert!(j.contains("第一行\\n第二行\\t制表"), "{j}");
        assert!(j.contains("\"canonical\":null"), "{j}");
        assert!(j.ends_with('}'));
    }

    #[test]
    fn json_truncates_text() {
        let long = "a".repeat(Page::JSON_TEXT_LIMIT + 100);
        let j = page("http://a.com/", "", &long).to_json();
        assert!(j.contains(&"a".repeat(Page::JSON_TEXT_LIMIT)));
        assert!(!j.contains(&"a".repeat(Page::JSON_TEXT_LIMIT + 1)));
    }

    #[test]
    fn control_chars_become_u_escapes() {
        let j = crate::json_string("\u{1}\u{1f}");
        assert_eq!(j, "\"\\u0001\\u001f\"");
    }

    #[test]
    fn jsonl_sink_writes_one_line_per_row() {
        let dir = std::env::temp_dir().join(format!("claw-jsonl-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("out.jsonl");
        let sink = JsonlSink::create(&path).unwrap();
        sink.on_page(&page("http://a.com/1", "一", "正文"));
        sink.on_page(&page("http://a.com/2", "二", "正文"));
        sink.on_error(&Url::parse("http://a.com/3").unwrap(), "connect refused");
        assert_eq!(sink.rows(), 3);
        assert_eq!(sink.failed(), 0);
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("\"title\":\"一\""));
        assert!(lines[2].contains("\"error\":\"connect refused\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_sink_keeps_pages() {
        let s = CollectSink::new();
        s.on_page(&page("http://a.com/", "t", "x"));
        s.on_error(&Url::parse("http://a.com/bad").unwrap(), "e");
        assert_eq!(s.pages().len(), 1);
        assert_eq!(s.errors().len(), 1);
    }
}
