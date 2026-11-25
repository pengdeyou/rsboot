//! 迁移：把多个模块各自的迁移目录聚合成一份，再跑。
//!
//! ## 为什么要聚合
//!
//! 微服务拆开前是模块化单体，每个模块自带 `migrations/`。启动时如果按模块
//! 各自跑，会出现"模块 A 的 v2 依赖模块 B 的 v3"这种跨模块顺序问题。
//! 聚合到一份、按版本号全局升序执行，顺序就确定了。
//!
//! 代价是**版本号必须全局唯一**。`Migrator::new` 会在重复版本上直接报错——
//! 两个迁移共用版本号意味着"应用顺序"没有确定答案，不同机器会跑出不同结果。

use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::runtime::app::App;

impl App {
    /// 跑到最新。返回本次实际应用的版本号（空 Vec 表示已是最新）。
    ///
    /// 只在默认库上跑。多库各自迁移是另一个话题（每库独立的迁移历史），
    /// 需要时用 `with_db_named` 自己组织，不在 `migrate()` 里猜。
    pub fn migrate(&self) -> Result<Vec<i64>> {
        if self.migrations.is_empty() {
            return Ok(Vec::new());
        }

        let mut all = Vec::new();
        for dir in &self.migrations {
            let p = PathBuf::from(dir);
            if !p.is_dir() {
                return Err(Error::Config(format!(
                    "迁移目录不存在：{}。检查 `.migrate_on_start(\"{}\")` 的路径",
                    p.display(),
                    dir
                )));
            }
            let found = rsbatis::from_dir(&p).map_err(|e| {
                Error::Config(format!("读取迁移目录 {} 失败：{e}", p.display()))
            })?;
            all.extend(found);
        }
        if all.is_empty() {
            return Ok(Vec::new());
        }

        self.ctx.with_db(|db| {
            let mut m = rsbatis::Migrator::new(db, all).map_err(|e| {
                Error::Lifecycle(format!(
                    "迁移准备失败：{e}。多个模块的迁移版本号必须全局唯一"
                ))
            })?;
            m.up()
                .map_err(|e| Error::Lifecycle(format!("迁移执行失败：{e}")))
        })
    }

    /// 已登记的迁移目录。
    pub fn migration_dirs(&self) -> &[String] {
        &self.migrations
    }
}
