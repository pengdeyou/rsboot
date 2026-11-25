//! OpenAPI 3.1 文档生成（不引 utoipa）。
//!
//! ## 为什么自己写
//!
//! utoipa 的核心价值是过程宏 `#[utoipa::path]`：从函数签名生成 operation。
//! 但它把路由注册拆成两种写法（`#[utoipa::path]` 一套、`Router` 一套），
//! 两边容易漂移；而且它的 schema 层是另一套 `ToSchema`，和 `schemars` 打架。
//!
//! rsboot 的取舍：**路由仍是普通的 `.get(...)`，文档用 `.describe(...)` 挂在
//! 同一条路由上**。好处是文档与路由共用同一个 key（方法 + 路径），
//! 挂不上去会在注册期报错，不可能"文档写了、接口没了"。
//!
//! 代价：handler 的入参类型不会自动出现在文档里，要显式 `.request_schema::<T>()`。
//! 这是刻意的——Rust 没有运行时反射，硬要"自动"就得引入宏，
//! 而宏会让 `Router` 的注册语法变复杂（见 §14 扩展点里的取舍）。
//!
//! ## 3.1 而不是 3.0
//!
//! `schemars` 产出的 JSON Schema 是 draft 2020-12（`type: ["string","null"]`
//! 这类写法在 3.0 里非法）。OpenAPI 3.1 才正式允许 JSON Schema 2020-12，
//! 所以版本号写 `3.1.0` 是唯一自洽的选择。

use std::sync::Arc;

use serde_json::{json, Value};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::web::http::{Method, Status};
use crate::web::response::Response;
use crate::web::route::Router;

/// 一个参数的描述。`location` 用 OpenAPI 的三个合法值。
#[derive(Debug, Clone)]
pub struct ParamDoc {
    pub name: String,
    pub location: &'static str,
    pub required: bool,
    pub schema: Value,
    pub description: Option<String>,
}

impl ParamDoc {
    /// 路径参数（模板里的 `{id}`，天然 required）
    pub fn path(name: &str, schema: Value) -> Self {
        Self {
            name: name.to_string(),
            location: "path",
            required: true,
            schema,
            description: None,
        }
    }

    pub fn query(name: &str, schema: Value) -> Self {
        Self {
            name: name.to_string(),
            location: "query",
            required: false,
            schema,
            description: None,
        }
    }

    pub fn header(name: &str, schema: Value) -> Self {
        Self {
            name: name.to_string(),
            location: "header",
            required: false,
            schema,
            description: None,
        }
    }

    pub fn required(mut self, yes: bool) -> Self {
        self.required = yes;
        self
    }

    pub fn describe(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }
}

/// 一条路由的文档。
#[derive(Debug, Clone)]
pub struct RouteDoc {
    pub summary: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub params: Vec<ParamDoc>,
    /// 请求体 schema（`application/json`）
    pub request: Option<Value>,
    /// 成功响应 schema
    pub response: Option<Value>,
    /// 成功状态码，默认 200
    pub success_status: u16,
    /// 额外错误：(状态码, 说明)
    pub errors: Vec<(u16, String)>,
}

impl RouteDoc {
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            description: None,
            tags: Vec::new(),
            params: Vec::new(),
            request: None,
            response: None,
            success_status: 200,
            errors: Vec::new(),
        }
    }

    pub fn describe(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }

    pub fn tag(mut self, t: impl Into<String>) -> Self {
        self.tags.push(t.into());
        self
    }

    pub fn param(mut self, p: ParamDoc) -> Self {
        self.params.push(p);
        self
    }

    /// 请求体 schema 直接给 `Value`（自定义/手写 schema 用）。
    pub fn request(mut self, schema: Value) -> Self {
        self.request = Some(schema);
        self
    }

    /// 请求体 schema 由类型推导。
    pub fn request_schema<T: schemars::JsonSchema>(self) -> Self {
        self.request(schema_of::<T>())
    }

    pub fn response(mut self, schema: Value) -> Self {
        self.response = Some(schema);
        self
    }

    pub fn response_schema<T: schemars::JsonSchema>(self) -> Self {
        self.response(schema_of::<T>())
    }

    pub fn success_status(mut self, code: u16) -> Self {
        self.success_status = code;
        self
    }

    pub fn error(mut self, code: u16, desc: impl Into<String>) -> Self {
        self.errors.push((code, desc.into()));
        self
    }
}

/// 从类型生成 JSON Schema。
///
/// 去掉 `$schema`：schemars 会在根上加 `"$schema": "https://json-schema.org/draft/2020-12/schema"`，
/// 而 OpenAPI 里每个 `schema` 对象是**内联**的，带上这个键会被一部分校验器判为非法。
pub fn schema_of<T: schemars::JsonSchema>() -> Value {
    let mut v = serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| json!({}));
    if let Value::Object(m) = &mut v {
        m.remove("$schema");
        m.remove("$id");
    }
    v
}

/// 文档头部信息。
#[derive(Debug, Clone)]
pub struct OpenApi {
    pub title: String,
    pub version: String,
    pub description: Option<String>,
    /// `servers[].url`，不填就不写这一段（相对当前地址即可）
    pub servers: Vec<String>,
}

impl OpenApi {
    pub fn new(title: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            version: version.into(),
            description: None,
            servers: Vec::new(),
        }
    }

    /// 从配置生成：`app.name`（默认 `rsboot API`）、`app.version`（默认 crate 版本）。
    pub fn from_config(config: &Config) -> Self {
        Self {
            title: config.get("app.name").unwrap_or("rsboot API").to_string(),
            version: config
                .get("app.version")
                .unwrap_or(crate::VERSION)
                .to_string(),
            description: config.get("app.description").map(str::to_string),
            servers: config
                .get("app.server")
                .map(|s| vec![s.to_string()])
                .unwrap_or_default(),
        }
    }

    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }

    pub fn server(mut self, url: impl Into<String>) -> Self {
        self.servers.push(url.into());
        self
    }

    /// 生成完整文档。
    pub fn document(&self, router: &Router) -> Value {
        let mut paths = serde_json::Map::new();

        for r in router.routes() {
            let key = r.template.raw.clone();
            let entry = paths
                .entry(key)
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if let Value::Object(m) = entry {
                m.insert(
                    r.method.as_str().to_ascii_lowercase(),
                    self.operation(r.method, &r.template, r.doc.as_ref()),
                );
            }
        }

        let mut root = json!({
            "openapi": "3.1.0",
            "info": {
                "title": self.title,
                "version": self.version,
            },
            "paths": Value::Object(paths),
        });
        if let Some(d) = &self.description {
            root["info"]["description"] = json!(d);
        }
        if !self.servers.is_empty() {
            root["servers"] = json!(self
                .servers
                .iter()
                .map(|u| json!({ "url": u }))
                .collect::<Vec<_>>());
        }
        root
    }

    fn operation(
        &self,
        method: Method,
        template: &crate::web::path::PathTemplate,
        doc: Option<&RouteDoc>,
    ) -> Value {
        // 没写文档的路由：从模板里推出路径参数（类型一律 string），
        // 这样至少客户端能生成调用代码，而不是完全不可用。
        let fallback_params: Vec<Value> = template
            .param_names()
            .into_iter()
            .map(|n| {
                json!({
                    "name": n,
                    "in": "path",
                    "required": true,
                    "schema": { "type": "string" },
                })
            })
            .collect();

        let Some(d) = doc else {
            return json!({
                "summary": format!("{} {}", method.as_str(), template.raw),
                "parameters": fallback_params,
                "responses": { "200": { "description": "OK" } },
            });
        };

        let params: Vec<Value> = d
            .params
            .iter()
            .map(|p| {
                let mut o = json!({
                    "name": p.name,
                    "in": p.location,
                    "required": p.required,
                    "schema": p.schema,
                });
                if let Some(desc) = &p.description {
                    o["description"] = json!(desc);
                }
                o
            })
            .collect();

        let mut op = json!({
            "summary": d.summary,
            "tags": d.tags,
            "parameters": params,
            "responses": self.responses(d),
        });
        if let Some(desc) = &d.description {
            op["description"] = json!(desc);
        }
        if let Some(req) = &d.request {
            op["requestBody"] = json!({
                "required": true,
                "content": { "application/json": { "schema": req } },
            });
        }
        // 文档里写了 tags 为空时不该留空数组（部分生成器会当成"无 tag"再分组一次）
        if d.tags.is_empty() {
            op.as_object_mut().unwrap().remove("tags");
        }
        op
    }

    fn responses(&self, d: &RouteDoc) -> Value {
        let mut m = serde_json::Map::new();
        let code = d.success_status.to_string();
        let mut ok = json!({ "description": "成功" });
        if let Some(s) = &d.response {
            ok["content"] = json!({ "application/json": { "schema": s } });
        }
        m.insert(code, ok);

        // 校验失败是每个带参数的接口都会发生的，自动补上
        let needs_422 = d.params.iter().any(|p| !p.location.eq_ignore_ascii_case("path"))
            || d.request.is_some();
        if needs_422 {
            m.insert(
                "422".into(),
                json!({ "description": "参数校验失败" }),
            );
        }
        for (c, desc) in &d.errors {
            m.insert(
                c.to_string(),
                json!({ "description": desc }),
            );
        }
        Value::Object(m)
    }
}

/// 把 `/openapi.json` 与 `/docs` 挂到路由上。
///
/// 使用者自己注册过同名路由就**尊重使用者的**，不覆盖、不报错——
/// 有人会想自己控制文档（比如只在内部网段暴露）。
pub fn attach(router: Router, spec: OpenApi) -> Router {
    let document = serde_json::to_vec(&spec.document(&router)).unwrap_or_else(|_| b"{}".to_vec());
    let bytes = Arc::new(document);

    let mut router = router;
    // 这两条路径是写死的字面量（不含 `{}`），`PathTemplate::parse` 不会失败；
    // 重复注册也已被上面的 `contains` 排除。所以这里用 `expect` 是"不可达断言"，
    // 而不是"忽略错误"——真到了这里说明框架自身有 bug，应该响。
    if !router.contains(Method::Get, "/openapi.json") {
        let b = Arc::clone(&bytes);
        router = router
            .get("/openapi.json", move |_, _| Response::json_with(&b, Status::OK))
            .expect("内置路由 /openapi.json 注册失败（路径是字面量，不应发生）");
    }
    if !router.contains(Method::Get, "/docs") {
        router = router
            .get("/docs", |_, _| Response::html(DOCS_HTML))
            .expect("内置路由 /docs 注册失败（路径是字面量，不应发生）");
    }
    router
}

/// 自研文档页。刻意不是 Swagger UI：
/// 引 swagger-ui 要带 ~1MB 静态资产或指到 CDN，而"内网部署打不开 CDN"
/// 是很常见的现场，页面上只剩一片空白。
pub const DOCS_HTML: &str = include_str!("docs.html");

/// 校验文档是否自洽：路径参数的声明必须与模板一致。
///
/// 这条容易错：模板写了 `{id}` 却忘了在 `.param()` 里声明，生成的文档会让
/// 客户端以为不需要传 id。所以提供一个可选的严格检查，测试与 CI 里调用。
pub fn validate(router: &Router) -> Result<()> {
    let mut errs = Vec::new();
    for r in router.routes() {
        let Some(doc) = &r.doc else { continue };
        let declared: Vec<String> = doc
            .params
            .iter()
            .filter(|p| p.location == "path")
            .map(|p| p.name.clone())
            .collect();
        for name in r.template.param_names() {
            if !declared.iter().any(|d| *d == name) {
                errs.push(format!(
                    "{} {} 的模板里有路径参数 `{name}`，但 `.describe()` 的 `.param()` 没声明它",
                    r.method.as_str(),
                    r.template.raw
                ));
            }
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(Error::Config(errs.join("；")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Environment;
    use crate::runtime::context::Context;
    use crate::web::request::Request;

    fn ctx() -> Context {
        Context::new(Config::new(), vec![], None, Environment::Dev)
    }

    fn call(router: &Router, method: Method, path: &str) -> (Status, String) {
        let c = ctx();
        let mut req = Request::new(method, path);
        let resp = router.handle(&mut req, &c);
        (
            resp.status,
            String::from_utf8_lossy(&resp.body).to_string(),
        )
    }

    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct Payload {
        name: String,
        age: u32,
    }

    #[test]
    fn schema_has_no_schema_key() {
        let s = schema_of::<Payload>();
        assert!(s.get("$schema").is_none(), "内联 schema 不该带 $schema：{s}");
        assert!(s.get("properties").is_some());
    }

    #[test]
    fn document_lists_undocumented_routes_too() {
        let router = Router::new()
            .get("/undocumented", |_, _| Response::text("x"))
            .unwrap()
            .get("/users/{id}", |_, _| Response::text("y"))
            .unwrap();
        let doc = OpenApi::new("t", "1").document(&router);
        let paths = doc["paths"].as_object().unwrap();
        assert!(paths.contains_key("/undocumented"), "没写文档的路由也要出现在 paths 里");
        assert!(paths.contains_key("/users/{id}"));
        assert_eq!(doc["openapi"], "3.1.0");
    }

    #[test]
    fn documented_route_carries_summary_params_and_schemas() {
        let router = Router::new()
            .get("/users/{id}", |_, _| Response::text("x"))
            .unwrap()
            .post("/users", |_, _| Response::text("x"))
            .unwrap()
            .describe(
                Method::Get,
                "/users/{id}",
                RouteDoc::new("查用户")
                    .tag("用户")
                    .param(ParamDoc::path("id", json!({ "type": "integer" })))
                    .response(json!({ "type": "object" })),
            )
            .unwrap()
            .describe(
                Method::Post,
                "/users",
                RouteDoc::new("建用户")
                    .request_schema::<Payload>()
                    .success_status(201),
            )
            .unwrap();

        let d = OpenApi::new("t", "1").document(&router);
        let get = &d["paths"]["/users/{id}"]["get"];
        assert_eq!(get["summary"], "查用户");
        assert_eq!(get["tags"][0], "用户");
        assert_eq!(get["parameters"][0]["in"], "path");
        // 只有路径参数时没有"校验失败"可谈（路径参数不合法等于 404），不补 422
        assert!(get["responses"].get("422").is_none());
        assert!(get["responses"]["200"]["content"]["application/json"]["schema"].is_object());

        let post = &d["paths"]["/users"]["post"];
        assert_eq!(post["responses"]["201"]["description"], "成功");
        assert!(post["responses"]["422"]["description"].is_string(), "有请求体就必须补 422");
        assert!(post["requestBody"]["content"]["application/json"]["schema"]
            .get("$schema")
            .is_none());
    }

    #[test]
    fn describe_on_missing_route_is_an_error() {
        let e = Router::new()
            .get("/a", |_, _| Response::text("x"))
            .unwrap()
            .describe(Method::Post, "/a", RouteDoc::new("x"))
            .unwrap_err();
        assert!(e.to_string().contains("没有 POST /a"), "{e}");
    }

    #[test]
    fn validate_catches_undeclared_path_param() {
        let router = Router::new()
            .get("/u/{id}/{slug}", |_, _| Response::text("x"))
            .unwrap()
            .describe(
                Method::Get,
                "/u/{id}/{slug}",
                RouteDoc::new("x").param(ParamDoc::path("id", json!({}))),
            )
            .unwrap();
        let e = validate(&router).unwrap_err();
        assert!(e.to_string().contains("slug"), "{e}");
    }

    #[test]
    fn attach_serves_spec_and_docs() {
        let router = attach(
            Router::new().get("/a", |_, _| Response::text("x")).unwrap(),
            OpenApi::new("我的 API", "2.0"),
        );
        let (s, body) = call(&router, Method::Get, "/openapi.json");
        assert_eq!(s, Status::OK);
        assert!(body.contains("\"我的 API\""), "{body}");

        let (s, body) = call(&router, Method::Get, "/docs");
        assert_eq!(s, Status::OK);
        assert!(body.contains("<html"), "{body}");
        assert!(body.contains("openapi.json"), "文档页要自己去拉 spec");
    }

    #[test]
    fn attach_does_not_override_user_route() {
        // 使用者自己写了 /docs：以他的为准（他可能想加鉴权或改样式）
        let router = attach(
            Router::new()
                .get("/docs", |_, _| Response::text("自建文档"))
                .unwrap(),
            OpenApi::new("t", "1"),
        );
        let (_, body) = call(&router, Method::Get, "/docs");
        assert_eq!(body, "自建文档");
        // /openapi.json 仍然补上
        assert!(router.contains(Method::Get, "/openapi.json"));
    }
}
