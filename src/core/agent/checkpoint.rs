use super::*;

/// 任务结束时的改动统计条：先补一次「结束快照」提交（让任务内新建文件也进影子仓库），
/// 再算相对基线的 numstat；无改动不发。撤销 = 影子仓库 reset --hard 回基线。
pub(crate) async fn emit_checkpoint_bar(
    baseline: Option<String>,
    session_id: &str,
    on_event: &(dyn Fn(AgentEvent) + Send + Sync),
) {
    let Some(base) = baseline else { return };
    let ws = tools::workspace_root().to_path_buf();
    let sid = session_id.to_string();
    let res = tokio::task::spawn_blocking({
        let base = base.clone();
        move || -> anyhow::Result<(usize, u64, u64)> {
            recovery::checkpoint(&ws, "任务结束快照", &sid)?;
            recovery::diff_stat(&ws, &base)
        }
    })
    .await;
    match res {
        Ok(Ok((files, adds, dels))) if files > 0 => {
            observability::event(
                EventType::State,
                json!({ "kind": "checkpoint.summary", "baseline": base, "files": files, "additions": adds, "deletions": dels }),
            );
            on_event(AgentEvent::Checkpoint { id: base, files_changed: files, additions: adds, deletions: dels });
        }
        Ok(Ok(_)) => {}
        Ok(Err(e)) => observability::event(
            EventType::State,
            json!({ "kind": "checkpoint.skipped", "reason": e.to_string() }),
        ),
        Err(e) => observability::event(
            EventType::State,
            json!({ "kind": "checkpoint.skipped", "reason": format!("任务失败: {e}") }),
        ),
    }
}
