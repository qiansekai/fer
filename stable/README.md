# stable — 在役稳定版备份

> 目的：开发迭代期间保一个「已知可用」的 fer.exe，随时可回退。
> 当前 `$env:FER` 仍指向 `target-gnu\release\fer.exe`（开发版）；此目录为回退锚点。

## 当前备份

| 项 | 值 |
|---|---|
| 文件 | `stable\fer.exe` |
| SHA256 | `6DDC5315A4E587976121B992ACFC876BB48CA5B52BE492BD89B012EB2720D029` |
| 备份时间 | 2026-09-01（v6 重建验证通过后，commit `ae9d187`） |
| 来源 commit | `ae9d187`（dump v6 alloc 段 + du --allocated + $MFT AllocatedSize） |
| dump 格式 | **FERIDX01 v6**（trigram 倒排段 + alloc 段；兼容加载 v3/v4/v5） |
| 已知性能 | 查询 serve 稳态子串 0-23ms/CJK 0-1ms；du 整卷 ~1.2s/子树 35ms；索引构建 9.1s/峰值 RSS 1.4GB |

> 配套环境：2026-09-01 已用本版本执行全盘 `fer index`（method: mft），当前
> 默认索引库即 v6 dump（418 万条）。回退到更老 exe 前注意：v6 dump 只有本版本
> 及以后能读，老 exe 需同时恢复 `%LOCALAPPDATA%\file-engine-rust\` 下的
> `.feridx.v5-bak-*` 备份（重命名为 `index.db.feridx`）。

## 回退方法

```powershell
Copy-Item D:\Kita-Tools\Coding\File-Engine-Rust\stable\fer.exe D:\Kita-Tools\Coding\File-Engine-Rust\target-gnu\release\fer.exe -Force
```

注意：**本备份能读 v3/v4/v5 三种 dump**（v3 自动内存重建加速段并提示 `fer upgrade`）。
若回退到更老的版本（v4 之前的 commit），v5 dump 无法加载，需重跑 `fer index`。

## 版本更替约定

- 每次「大改前」把当时在用的 release 覆盖进本目录，并更新本表（来源 commit + dump 版本）。
- 验证新版本稳定后，若确认不再需要旧备份，可直接覆盖更新，不必保留多份。
