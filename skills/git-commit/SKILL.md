---
name: git-commit
description: 生成符合约定 git 提交信息与提交流程
---

# git-commit 技能

1. 先运行 git status 与 git diff 查看全部待提交改动
2. 提交信息格式：第一行不超过 50 字的中文概述，空一行后写要点列表
3. 每个逻辑改动单独一次 commit，绝不 git add . 整体提交
4. 提交前运行项目测试，失败则先修复
