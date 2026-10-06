---
name: github
description: 用 gh CLI 操作 GitHub：PR/issue/release/CI
---

# GitHub 技能（gh CLI）

适用：看仓库/PR/issue/CI 状态、建 PR、回 issue、发 release、查 Actions 日志等 GitHub 操作。
纯本地 git 操作（commit/branch/merge）用 `git` 工具就够，不要绕道 gh。

## 0. 环境检测（先跑，诚实报错）

1. `gh --version` —— 不存在：告知用户「未安装 gh CLI，从 https://cli.github.com/ 安装后重试」；
   期间可用 git + web_fetch（api.github.com 只读接口）兜底，但**别假装完成了 gh 能做的写操作**。
2. `gh auth status` —— 未登录：认证是用户本人的一次性动作，让用户在终端跑 `gh auth login`，
   或提供 `GH_TOKEN` 环境变量；**不要索要、猜测或落盘保存任何 token**。

## 1. 常用查询（只读，放手做）

- 仓库：`gh repo view <owner/repo>`、`gh repo view --web`
- PR：`gh pr list`、`gh pr view <n>`、`gh pr diff <n>`、`gh pr checks <n>`
- issue：`gh issue list`、`gh issue view <n> --comments`
- CI：`gh run list --limit 5`、`gh run view <id> --log-failed`
- release：`gh release list`、`gh release view <tag>`

## 2. 写操作（外发动作，先确认再执行）

以下动作会把内容发布到 GitHub，**每次都先向用户复述将发生什么并得到确认**（与本产品 git push 审批同级）：

- `gh pr create --title … --body …`（先 `git diff` 给用户看要进 PR 的提交）
- `gh pr merge <n>`（说明合并方式；绝不 `--force` 删除分支类破坏项）
- `gh issue create / issue comment / issue close`
- `gh release create <tag>`（附上产物前先核对文件清单）
- `gh repo create / fork / delete`（delete 是不可逆动作，必须用户逐次明确授权）

## 3. 纪律

1. 输出只报事实：PR/issue 链接、run 结果、失败日志摘要；失败时贴 gh 的原始报错，不编造状态。
2. 批量操作（给多个 issue 打标签等）先列清单再执行。
3. 提交信息规范走 git-commit 技能；PR 标题用一句话中文概述，正文写要点列表。
