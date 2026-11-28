//! 跑一遍就能看到所有语法：`cargo run -p rsboot-view --example render`
//!
//! 不连数据库、不起服务：只把一段页面渲染出来打到终端。

use rsboot_view::Engine;
use serde_json::json;

fn main() {
    let engine = Engine::new()
        .globals(&json!({ "site": { "name": "rsboot 后台" } }))
        .func("money", |args| {
            let v = args
                .iter()
                .find(|(k, _)| *k == "value")
                .and_then(|(_, v)| v.as_f64())
                .unwrap_or(0.0);
            Ok(json!(format!("¥{v:.2}")))
        })
        .add(
            "layout",
            "<html><head><title>{block name=\"title\"}首页{/block}</title></head>\
             <body>{block name=\"body\"}{/block}</body></html>",
        )
        .unwrap()
        .add(
            "list",
            "{extends \"layout\"}\
             {block name=\"title\"}{$site.name} · 订单{/block}\
             {block name=\"body\"}\
             <p>共 {$rows|length} 单，起止 {$from|date:\"%Y-%m-%d\"} ~ {$to|date:\"%Y-%m-%d\"}</p>\
             {html_select name=\"status\" options=$statuses selected=$cur empty=\"全部\"}\
             <table>{foreach $rows as $r}\
             <tr class=\"{cycle values=\"odd,even\"}\"><td>{$r@iteration}</td><td>{$r.name|escape}</td>\
             <td>{money value=$r.amount}</td><td>{if $r@last}末行{else}{$r@index}{/if}</td></tr>\
             {/foreach}</table>\
             {capture name=\"sum\"}{for $i=1 to 3}{$i}{/for}{/capture}\
             <p>片段：{$sum|raw}；编号：{$code|pad:5}</p>\
             {/block}",
        )
        .unwrap();

    let data = json!({
        "from": 1735689600, "to": 1738368000,
        "cur": "paid",
        "statuses": [{"value": "all", "label": "全部"}, {"value": "paid", "label": "已付款"}],
        "code": 42,
        "rows": [
            {"name": "张三 <script>", "amount": 88.5},
            {"name": "李四", "amount": 1200}
        ],
    });
    println!("{}", engine.render("list", &data).unwrap());

    // 运行期错误也会指出模板和行号
    let broken = Engine::new().add("bad", "ok\n\n{$x|nope}").unwrap();
    let err = broken.render("bad", &json!({"x": 1})).unwrap_err();
    eprintln!("预期的错误：{err}");
}
