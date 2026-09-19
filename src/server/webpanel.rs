//! 网页面板后端（P20.1）——右侧「网页」面板：多标签管理 + 截图渲染 + 坐标回传。
//!
//! 与 agent 的 browser_* 工具共享同一无头浏览器（多标签互不干扰）；面板不走审批门
//! （用户亲自操作，非模型调用），但照受全局来源校验中间件保护（仅本机回环可访问）。

use super::*;
use axum::response::IntoResponse;

/// GET /api/browser/state —— 浏览器是否在跑 + 全部标签页（id/标题/地址/是否 agent 页）。
pub(super) async fn browser_state() -> Json<Value> {
    Json(crate::browser::panel_state().await)
}

/// POST /api/browser/new —— 开新标签页（body 可带 url，缺省 about:blank）。
pub(super) async fn browser_new(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let url = req.get("url").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    match crate::browser::panel_new(url.map(str::to_string)).await {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err(bad(e)),
    }
}

fn bad(e: String) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": e })))
}

/// POST /api/browser/navigate —— 地址栏回车（id + url）。
pub(super) async fn browser_navigate(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    let Some(url) = req.get("url").and_then(Value::as_str) else { return Err(bad("缺少 url".into())) };
    match crate::browser::panel_navigate(id, url).await {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err(bad(e)),
    }
}

/// GET /api/browser/screenshot?id= —— 当前视口 PNG（前端 <img> 直链，动作后换参数刷新）。
pub(super) async fn browser_screenshot(Query(q): Query<ScreenshotQuery>) -> Result<impl IntoResponse, (StatusCode, Json<Value>)> {
    match crate::browser::panel_screenshot(&q.id).await {
        Ok(png) => Ok((
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "image/png")],
            png,
        )),
        Err(e) => Err(bad(e)),
    }
}

#[derive(Debug, Deserialize)]
pub struct ScreenshotQuery {
    pub id: String,
}

async fn click_impl(id: &str, req: &Value) -> Result<Value, String> {
    let x = req.get("x").and_then(Value::as_i64).unwrap_or(0);
    let y = req.get("y").and_then(Value::as_i64).unwrap_or(0);
    crate::browser::panel_click(id, x, y).await
}

/// POST /api/browser/click
pub(super) async fn browser_click(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    click_impl(id, &req).await.map(Json).map_err(bad)
}

async fn type_impl(id: &str, req: &Value) -> Result<Value, String> {
    let text = req.get("text").and_then(Value::as_str).unwrap_or("");
    crate::browser::panel_type(id, text).await
}

/// POST /api/browser/type
pub(super) async fn browser_type(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    type_impl(id, &req).await.map(Json).map_err(bad)
}

async fn key_impl(id: &str, req: &Value) -> Result<Value, String> {
    let key = req.get("key").and_then(Value::as_str).unwrap_or("Enter");
    let code = req.get("code").and_then(Value::as_i64).unwrap_or(13);
    crate::browser::panel_key(id, key, code).await
}

/// POST /api/browser/key
pub(super) async fn browser_key(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    key_impl(id, &req).await.map(Json).map_err(bad)
}

async fn scroll_impl(id: &str, req: &Value) -> Result<Value, String> {
    let dy = req.get("deltaY").and_then(Value::as_i64).unwrap_or(0);
    crate::browser::panel_scroll(id, dy).await
}

/// POST /api/browser/scroll
pub(super) async fn browser_scroll(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    scroll_impl(id, &req).await.map(Json).map_err(bad)
}

/// POST /api/browser/back
pub(super) async fn browser_back(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    crate::browser::panel_back(id).await.map(Json).map_err(bad)
}

/// POST /api/browser/forward
pub(super) async fn browser_forward(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    crate::browser::panel_forward(id).await.map(Json).map_err(bad)
}

/// POST /api/browser/reload
pub(super) async fn browser_reload(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    crate::browser::panel_reload(id).await.map(Json).map_err(bad)
}

/// DELETE /api/browser/tab?id= —— 关标签页。
pub(super) async fn browser_close_tab(Query(q): Query<ScreenshotQuery>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match crate::browser::panel_close_tab(&q.id).await {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err(bad(e)),
    }
}
