//! 生命周期：启动、关闭、异常回退、一次性运行。
//!
//! 三条不变量：
//! 1. **启动失败必须退回去**。半启动状态（进程活着、一半模块在跑）最难查。
//! 2. **关闭要收集所有错误再返回**。第一个 `stop` 失败就中断，后面的模块永远关不掉，
//!    连接与后台线程会泄漏。
//! 3. **业务出错也要关闭**。`run_until` 里 body 返回 `Err` 时照样执行关闭。

use crate::error::{Error, Result};
use crate::runtime::app::App;
use crate::runtime::context::Context;

impl App {
    /// 启动：先跑迁移（若开启），再按注册顺序启动模块。
    pub fn start(&mut self) -> Result<()> {
        // panic hook 只装一次：日志里带上线程名与请求 trace
        crate::web::exception::install_panic_hook();
        // 热加载先起：迁移/模块启动期间改配置也能被接住
        if let Some(plan) = &self.reload {
            match crate::config::watcher::Watcher::spawn(self.ctx.config_slot(), plan.to_spec())
            {
                Ok(w) => self.watcher = Some(w),
                Err(e) => {
                    return Err(self.unwind(Error::Lifecycle(format!(
                        "启动配置热加载失败：{e}"
                    ))))
                }
            }
        }
        if self.migrate_on_start {
            if let Err(e) = self.migrate() {
                return Err(self.unwind(e));
            }
        }
        for i in 0..self.modules.len() {
            let name = self.modules[i].name();
            if let Err(e) = self.modules[i].start(&self.ctx) {
                let orig = Error::Lifecycle(format!("模块 `{name}` 启动失败：{e}"));
                return Err(self.unwind(orig));
            }
            self.started.push(i);
        }
        Ok(())
    }

    /// 回滚式关闭：只关已启动的，逆序。关闭本身出错时**不能覆盖**原始错误。
    fn unwind(&mut self, orig: Error) -> Error {
        match self.stop() {
            Ok(()) => orig,
            Err(e) => Error::Lifecycle(format!("{orig}；此外，失败后的关闭过程也出错：{e}")),
        }
    }

    /// 关闭：逆序，把所有错误收集起来一次性返回。
    pub fn stop(&mut self) -> Result<()> {
        // 先停热加载：模块关闭期间不该再换配置
        drop(self.watcher.take());
        let mut errs: Vec<String> = Vec::new();
        for i in self.started.iter().rev().copied() {
            let name = self.modules[i].name();
            if let Err(e) = self.modules[i].stop(&self.ctx) {
                errs.push(format!("模块 `{name}` 关闭失败：{e}"));
            }
        }
        self.started.clear();
        if errs.is_empty() {
            Ok(())
        } else {
            Err(Error::Lifecycle(errs.join("；")))
        }
    }

    /// 启动 → 跑一段业务 → 关闭。业务出错时照样关闭。
    ///
    /// 长驻服务（自己起事件循环）用 `start()` + `stop()` 显式管理。
    pub fn run_until(&mut self, body: impl FnOnce(&Context) -> Result<()>) -> Result<()> {
        self.start()?;
        let r = body(&self.ctx);
        let stopped = self.stop();
        match (r, stopped) {
            (Ok(()), Ok(())) => Ok(()),
            // 业务错误优先：那是用户真正关心的；关闭错误附在后面
            (Err(body_err), Ok(())) => Err(body_err),
            (Ok(()), Err(stop_err)) => Err(stop_err),
            (Err(body_err), Err(stop_err)) => Err(Error::Lifecycle(format!(
                "{body_err}；关闭时也出错：{stop_err}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::module::Module;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// 记录调用顺序的测试模块。
    struct Recorder {
        name: &'static str,
        log: Rc<RefCell<Vec<String>>>,
        fail_start: bool,
        fail_stop: bool,
    }

    impl Module for Recorder {
        fn name(&self) -> &'static str {
            self.name
        }
        fn start(&mut self, _ctx: &Context) -> Result<()> {
            self.log.borrow_mut().push(format!("start:{}", self.name));
            if self.fail_start {
                return Err(Error::Lifecycle("boom".into()));
            }
            Ok(())
        }
        fn stop(&mut self, _ctx: &Context) -> Result<()> {
            self.log.borrow_mut().push(format!("stop:{}", self.name));
            if self.fail_stop {
                return Err(Error::Lifecycle("stop-boom".into()));
            }
            Ok(())
        }
    }

    fn app_with(modules: Vec<Box<dyn Module>>) -> App {
        let mut b = App::builder().database("default", "sqlite://:memory:");
        for m in modules {
            b = b.module_boxed(m);
        }
        b.build().unwrap()
    }

    #[test]
    fn start_then_stop_is_reverse_order() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let app = app_with(vec![
            Box::new(Recorder { name: "a", log: log.clone(), fail_start: false, fail_stop: false }),
            Box::new(Recorder { name: "b", log: log.clone(), fail_start: false, fail_stop: false }),
        ]);
        let mut app = app;
        app.start().unwrap();
        assert_eq!(app.started_modules(), ["a", "b"]);
        app.stop().unwrap();
        assert_eq!(
            *log.borrow(),
            ["start:a", "start:b", "stop:b", "stop:a"],
            "关闭必须逆序"
        );
    }

    #[test]
    fn failed_start_rolls_back_started_ones() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut app = app_with(vec![
            Box::new(Recorder { name: "a", log: log.clone(), fail_start: false, fail_stop: false }),
            Box::new(Recorder { name: "b", log: log.clone(), fail_start: true, fail_stop: false }),
            Box::new(Recorder { name: "c", log: log.clone(), fail_start: false, fail_stop: false }),
        ]);
        let e = app.start().unwrap_err();
        assert!(e.to_string().contains("模块 `b` 启动失败"), "{e}");
        // a 被启动过就必须被关闭；b 失败没进 started；c 根本没启动
        assert_eq!(*log.borrow(), ["start:a", "start:b", "stop:a"]);
        assert!(!app.is_running());
    }

    #[test]
    fn stop_collects_all_errors_instead_of_stopping_early() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut app = app_with(vec![
            Box::new(Recorder { name: "a", log: log.clone(), fail_start: false, fail_stop: true }),
            Box::new(Recorder { name: "b", log: log.clone(), fail_start: false, fail_stop: true }),
        ]);
        app.start().unwrap();
        let e = app.stop().unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("`b`"), "必须报出 b：{msg}");
        assert!(msg.contains("`a`"), "也必须报出 a（不能因 b 失败就跳过）：{msg}");
        assert_eq!(*log.borrow(), ["start:a", "start:b", "stop:b", "stop:a"]);
    }

    #[test]
    fn run_until_stops_even_when_body_fails() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut app = app_with(vec![Box::new(Recorder {
            name: "a",
            log: log.clone(),
            fail_start: false,
            fail_stop: false,
        })]);
        let e = app
            .run_until(|_| Err(Error::Lifecycle("业务炸了".into())))
            .unwrap_err();
        assert!(e.to_string().contains("业务炸了"), "{e}");
        assert_eq!(*log.borrow(), ["start:a", "stop:a"], "出错也要关闭");
        assert!(!app.is_running());
    }

    #[test]
    fn double_stop_is_harmless() {
        let mut app = app_with(vec![]);
        app.start().unwrap();
        app.stop().unwrap();
        assert!(app.stop().is_ok(), "重复关闭不该报错");
        assert!(!app.is_running());
    }
}
