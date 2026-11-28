//! HTML → Markdown / 正文块。
//!
//! 爬下来的页面最终要喂给人看或喂给模型看，`Document::text` 那种"把所有可见
//! 文字压成一行"对这两件事都不够用：段落没了、链接没了、标题和正文糊在一起。
//!
//! 这里的做法仍是**一趟扫描、不建树**，只是把"块"的概念补上：
//!
//! - 块级标签（`p` / `h1`–`h6` / `li` / `blockquote` / `pre` / `tr` / `hr`）
//!   切断块，块内的行内标签（`a` / `strong` / `em` / `code` / `img`）在这一趟里
//!   直接渲染成 Markdown 标记。
//! - 容器标签（`div` / `section` / `table` …）透明：它们只切块、不加前缀。
//! - 列表用栈记缩进和序号，`ol` 自动数 `1. 2. 3.`；`blockquote` 嵌套加 `> `。
//! - 纯文本模式（`text_blocks` / `text_main`）复用同一个渲染器，只是不写标记，
//!   并且可以跳过 `nav` / `header` / `footer` / `aside` / `form` 这些噪音区。
//!
//! 明确不做：表格对齐行（`| :--- |` 那行）、嵌套表格、脚注、HTML 实体全集
//! （沿用 `html::unescape`）、`<pre>` 的语法高亮语言标注。

use crate::html::{attr, find, find_ignore_case, read_attrs, read_name, unescape};

/// 把 HTML 渲染成 Markdown。
pub fn to_markdown(html: &str) -> String {
    let mut r = Renderer::new(Mode::Markdown);
    r.run(html);
    r.markdown
}

/// 每个块一段，压缩空白，不写 Markdown 标记。噪音区（导航/页脚/表单）保留。
pub fn text_blocks(html: &str) -> Vec<String> {
    let mut r = Renderer::new(Mode::Text);
    r.run(html);
    r.blocks
}

/// 近似正文：块级文本，跳过 `nav` / `header` / `footer` / `aside` / `form`。
///
/// 这叫"近似"而不是 readability 级的正文抽取：真正的正文识别要看文本密度、
/// 链接密度、标签路径打分，那是另一个量级的实现。跳过页头页脚已经能吃掉
/// 网页里最大的一块噪声。
pub fn text_main(html: &str) -> String {
    let mut r = Renderer::new(Mode::Main);
    r.run(html);
    r.blocks.join("\n\n")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Markdown,
    Text,
    Main,
}

struct Renderer {
    mode: Mode,
    markdown: String,
    blocks: Vec<String>,
    /// 当前块的缓冲
    line: String,
    /// 正在处理的标题层级，0 表示不是标题
    heading: u8,
    /// 待消费的列表项前缀（缩进, 标记）
    li: Option<(usize, String)>,
    /// `(是否有序, 已用序号)`
    lists: Vec<(bool, usize)>,
    quote: usize,
    /// 噪音区深度
    noise: usize,
    in_row: bool,
    row: Vec<String>,
    link_href: Option<String>,
    /// `(标记, 开标记前的行长)`——用来识别"空标记"并回退
    marks: Vec<(&'static str, usize)>,
    last_li: bool,
}

impl Renderer {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            markdown: String::new(),
            blocks: Vec::new(),
            line: String::new(),
            heading: 0,
            li: None,
            lists: Vec::new(),
            quote: 0,
            noise: 0,
            in_row: false,
            row: Vec::new(),
            link_href: None,
            marks: Vec::new(),
            last_li: false,
        }
    }

    fn md(&self) -> bool {
        self.mode == Mode::Markdown
    }

    fn run(&mut self, html: &str) {
        let b = html.as_bytes();
        let mut i = 0usize;
        while i < b.len() {
            if b[i] != b'<' {
                let start = i;
                while i < b.len() && b[i] != b'<' {
                    i += 1;
                }
                self.text(&unescape(&html[start..i]));
                continue;
            }
            if b[i..].starts_with(b"<!--") {
                match find(b, i + 4, b"-->") {
                    Some(e) => {
                        i = e + 3;
                        continue;
                    }
                    None => break,
                }
            }
            if b[i..].starts_with(b"<!") || b[i..].starts_with(b"?") {
                match find(b, i + 2, b">") {
                    Some(e) => {
                        i = e + 1;
                        continue;
                    }
                    None => break,
                }
            }

            let closing = b.get(i + 1) == Some(&b'/');
            let name_at = if closing { i + 2 } else { i + 1 };
            let (name, after) = read_name(b, name_at);
            let nl = name.to_ascii_lowercase();
            let (attrs, next) = read_attrs(b, after);
            i = next;

            // 不产生可见文本的两类：脚本样式整段跳、pre 单独处理
            if nl == "script" || nl == "style" || nl == "noscript" {
                let close = format!("</{nl}");
                match find_ignore_case(b, i, close.as_bytes()) {
                    Some(e) => match find(b, e, b">") {
                        Some(gt) => i = gt + 1,
                        None => break,
                    },
                    None => break,
                }
                continue;
            }
            if nl == "pre" && !closing {
                i = self.pre(html, i);
                continue;
            }

            if closing {
                self.end_tag(&nl);
            } else {
                self.start_tag(&nl, &attrs);
            }
        }

        self.flush();
        self.end_row();
        self.markdown = self.markdown.trim().to_string();
        for blk in &mut self.blocks {
            *blk = blk.trim().to_string();
        }
        self.blocks.retain(|b| !b.is_empty());
    }

    fn text(&mut self, raw: &str) {
        if self.mode == Mode::Main && self.noise > 0 {
            return;
        }
        self.line.push_str(raw);
    }

    fn start_tag(&mut self, nl: &str, attrs: &[(String, String)]) {
        match nl {
            "nav" | "header" | "footer" | "aside" | "form" => {
                self.flush();
                self.noise += 1;
            }
            "p" | "dt" | "dd" | "figcaption" | "caption" | "div" | "section" | "article"
            | "main" | "table" | "thead" | "tbody" | "dl" | "figure" | "details" | "summary" => {
                self.flush();
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush();
                self.heading = nl.as_bytes()[1] - b'0';
            }
            "blockquote" => {
                self.flush();
                self.quote += 1;
            }
            "ul" | "ol" => {
                self.flush();
                self.lists.push((nl == "ol", 0));
            }
            "li" => {
                self.flush();
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some((true, n)) => {
                        *n += 1;
                        format!("{}. ", *n)
                    }
                    _ => "- ".to_string(),
                };
                self.li = Some((depth * 2, marker));
            }
            "tr" => {
                self.flush();
                self.in_row = true;
                self.row.clear();
            }
            "hr" => {
                self.flush();
                if self.md() {
                    self.push("---".into(), false);
                }
            }
            "br" => self.text(" "),
            "img" => {
                if self.md() {
                    let src = unescape(attr(attrs, "src").unwrap_or_default());
                    let alt = unescape(attr(attrs, "alt").unwrap_or_default());
                    let src = src.trim();
                    if !src.is_empty() {
                        let tag = format!("![{}]({})", alt.trim(), src);
                        self.text(&tag);
                    }
                }
            }
            "a" => {
                let href = attr(attrs, "href").unwrap_or_default();
                if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
                    return;
                }
                if self.md() {
                    self.text("[");
                    self.link_href = Some(unescape(href).trim().to_string());
                }
            }
            "strong" | "b" => self.open_mark("**"),
            "em" | "i" => self.open_mark("*"),
            "code" => self.open_mark("`"),
            "del" | "s" => self.open_mark("~~"),
            _ => {}
        }
    }

    fn end_tag(&mut self, nl: &str) {
        match nl {
            "nav" | "header" | "footer" | "aside" | "form" => {
                self.flush();
                self.noise = self.noise.saturating_sub(1);
            }
            "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote" | "li" | "dt" | "dd"
            | "figcaption" | "caption" | "div" | "section" | "article" | "main" | "table"
            | "thead" | "tbody" | "dl" | "figure" | "details" | "summary" => {
                self.flush();
                match nl {
                    "blockquote" => self.quote = self.quote.saturating_sub(1),
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => self.heading = 0,
                    _ => {}
                }
            }
            "ul" | "ol" => {
                self.flush();
                self.lists.pop();
                // 最外层列表结束：后面即使还是列表项，也与上一组之间留空行
                if self.lists.is_empty() {
                    self.last_li = false;
                }
            }
            "td" | "th" => self.end_cell(),
            "tr" => self.end_row(),
            "a" => {
                if let Some(href) = self.link_href.take() {
                    if self.line.ends_with('[') {
                        // 没有锚文本（图片链接之类）：直接把地址当文本
                        self.line.pop();
                        self.text(&href);
                    } else {
                        let tag = format!("]({href})");
                        self.text(&tag);
                    }
                }
            }
            "strong" | "b" | "em" | "i" | "code" | "del" | "s" => self.close_mark(),
            _ => {}
        }
    }

    fn open_mark(&mut self, m: &'static str) {
        if !self.md() {
            return;
        }
        self.marks.push((m, self.line.len()));
        self.text(m);
    }

    fn close_mark(&mut self) {
        if !self.md() {
            return;
        }
        if let Some((m, at)) = self.marks.pop() {
            // `<b></b>` 会输出成 `****`：比开标记还短就说明块内啥也没有，退回去
            if self.line.len() > at + m.len() {
                self.line.push_str(m);
            } else {
                self.line.truncate(at);
            }
        }
    }

    fn end_cell(&mut self) {
        let t = collapse(&self.line);
        self.line.clear();
        if self.in_row {
            self.row.push(t);
        } else if !t.is_empty() {
            self.push(t, false);
        }
    }

    fn end_row(&mut self) {
        if !self.in_row {
            return;
        }
        self.in_row = false;
        let cells = std::mem::take(&mut self.row);
        if !cells.iter().any(|c| !c.is_empty()) {
            return;
        }
        let joined = if self.md() {
            format!("| {} |", cells.join(" | "))
        } else {
            cells.join("  ")
        };
        self.push(joined, false);
    }

    fn flush(&mut self) {
        let t = collapse(&self.line);
        self.line.clear();
        let li = self.li.take();
        if t.is_empty() || (self.mode == Mode::Main && self.noise > 0) {
            return;
        }
        let is_li = li.is_some();
        let mut s = String::new();
        if let Some((indent, marker)) = li {
            s.push_str(&" ".repeat(indent));
            s.push_str(&marker);
        }
        for _ in 0..self.quote {
            s.push_str("> ");
        }
        if self.heading > 0 && self.md() {
            s.push_str(&"#".repeat(self.heading as usize));
            s.push(' ');
        }
        s.push_str(&t);
        self.push(s, is_li);
    }

    fn push(&mut self, s: String, is_li: bool) {
        self.blocks.push(s.clone());
        if self.md() {
            if !self.markdown.is_empty() {
                if is_li && self.last_li {
                    self.markdown.push('\n');
                } else {
                    self.markdown.push_str("\n\n");
                }
            }
            self.markdown.push_str(&s);
        }
        self.last_li = is_li;
    }

    /// `<pre>` 内部：保留空白，整块进代码围栏。返回原始 HTML 里的继续位置。
    fn pre(&mut self, html: &str, from: usize) -> usize {
        let b = html.as_bytes();
        let end = find_ignore_case(b, from, b"</pre").unwrap_or(b.len());
        let code = unescape(&strip_tags(&html[from..end]));
        let code = code.trim_matches('\n').trim_end().to_string();
        self.flush();
        let resume = match find(b, end, b">") {
            Some(gt) => gt + 1,
            None => b.len(),
        };
        if code.trim().is_empty() {
            return resume;
        }
        if self.md() {
            if !self.markdown.is_empty() {
                self.markdown.push_str("\n\n");
            }
            self.markdown.push_str("```\n");
            self.markdown.push_str(&code);
            self.markdown.push_str("\n```");
        }
        self.blocks.push(code);
        resume
    }
}

/// 去掉内部的标签（`pre` 里常见 `<code>` 包裹），保留文本与空白。
fn strip_tags(raw: &str) -> String {
    let b = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'<' {
            match find(b, i + 1, b">") {
                Some(e) => {
                    i = e + 1;
                    continue;
                }
                None => break,
            }
        }
        let start = i;
        while i < b.len() && b[i] != b'<' {
            i += 1;
        }
        out.push_str(&raw[start..i]);
    }
    out
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_paragraphs_and_emphasis() {
        let md = to_markdown(
            "<h1>标题</h1><p>这是 <strong>粗</strong> 和 <em>斜</em> 文本。</p>\
             <p>第二段</p>",
        );
        assert_eq!(md, "# 标题\n\n这是 **粗** 和 *斜* 文本。\n\n第二段");
    }

    #[test]
    fn links_and_images_become_markdown() {
        let md = to_markdown(
            "<p>见 <a href=\"/docs/a\">文档</a> 和 <a href=\"/x\"><img src=\"/i.png\" alt=\"图\"></a></p>",
        );
        assert_eq!(md, "见 [文档](/docs/a) 和 [![图](/i.png)](/x)");
    }

    #[test]
    fn lists_are_numbered_and_indented() {
        let md = to_markdown(
            "<ul><li>一<ul><li>一点一</li></ul></li><li>二</li></ul>\
             <ol><li>甲</li><li>乙</li></ol>",
        );
        assert_eq!(md, "- 一\n  - 一点一\n- 二\n\n1. 甲\n2. 乙");
    }

    #[test]
    fn pre_keeps_whitespace_in_a_fence() {
        let md = to_markdown("<p>代码：</p><pre><code>fn main() {\n    x();\n}</code></pre>");
        assert_eq!(md, "代码：\n\n```\nfn main() {\n    x();\n}\n```");
    }

    #[test]
    fn table_rows_are_pipes() {
        let md = to_markdown("<table><tr><th>名</th><th>值</th></tr><tr><td>a</td><td>1</td></tr></table>");
        assert_eq!(md, "| 名 | 值 |\n\n| a | 1 |");
    }

    #[test]
    fn blockquote_nests() {
        let md = to_markdown("<blockquote><p>外</p><blockquote><p>内</p></blockquote></blockquote>");
        assert_eq!(md, "> 外\n\n> > 内");
    }

    #[test]
    fn script_style_and_comments_are_dropped() {
        let md = to_markdown(
            "<p>前</p><script>var x = '<p>不该出现</p>';</script>\
             <style>.a{}</style><!-- 注释 --><p>后</p>",
        );
        assert_eq!(md, "前\n\n后");
    }

    #[test]
    fn empty_tags_do_not_emit_stray_marks() {
        assert_eq!(to_markdown("<p>a<b></b>b</p>"), "ab");
        assert_eq!(to_markdown("<p><span></span></p>"), "");
    }

    #[test]
    fn text_main_skips_navigation_and_footer() {
        let html = "<html><body>\
            <header><nav>菜单 一 二 三</nav></header>\
            <main><h1>真标题</h1><p>正文第一段</p><p>正文第二段</p></main>\
            <footer>版权 2026</footer>\
            <aside>侧栏广告</aside></body></html>";
        let main_text = text_main(html);
        assert!(main_text.contains("真标题") && main_text.contains("正文第二段"));
        assert!(!main_text.contains("菜单"), "{main_text}");
        assert!(!main_text.contains("版权") && !main_text.contains("广告"));

        // text_blocks 不跳噪音区，但也不带标记
        let blocks = text_blocks(html);
        assert!(blocks.iter().any(|b| b.contains("菜单")));
        assert!(blocks.iter().any(|b| b.contains("真标题")));
        assert!(!blocks.iter().any(|b| b.contains("#")));
    }

    #[test]
    fn entities_are_decoded_in_output() {
        assert_eq!(to_markdown("<p>a &amp; b &lt; c</p>"), "a & b < c");
    }

    #[test]
    fn unclosed_tags_do_not_loop_forever() {
        // 这是零依赖扫描器最容易翻车的地方：不闭合的标签必须能走到结尾
        let md = to_markdown("<div><p>一<ul><li>二<pre>三");
        assert!(md.contains('一'), "{md}");
        assert!(to_markdown("<p><a href=x").len() >= 0);
    }
}
