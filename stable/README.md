# stable — 在役稳定版备份

> 目的：开发迭代期间保一个「已知可用」的 fer.exe，随时可回退。
> 当前 `$env:FER` 仍指向 `target-gnu\release\fer.exe`（开发版）；此目录为回退锚点。

## 当前备份

| 项 | 值 |
|---|---|
| 文件 | `stable\fer.exe`（2,916,705 字节） |
| SHA256 | `F2CB104D8765D365337FF41B3C7B05E81DC5FCFC17CB0E4401858AEFD9202D1B` |
| 备份时间 | 2026-09-01 |
| 来源 commit | `4ef24c2`（2026-08-31，feat: regex 查询收尾） |
| dump 格式 | **FERIDX01 v3**（与当时 `%LOCALAPPDATA%\file-engine-rust\index.db.feridx` 匹配） |
| 已知性能 | 全查询 0-294ms（含进程启动），2 字子串 ~152-168ms，serve 启动 86ms |

## 回退方法

```powershell
Copy-Item D:\Kita-Tools\Coding\File-Engine-Rust\stable\fer.exe D:\Kita-Tools\Coding\File-Engine-Rust\target-gnu\release\fer.exe -Force
```

注意：**v3 dump 与新版本（v4）互不兼容**。若回退到本备份但索引已用新版重建为 v4，
需重跑 `fer index`（管理员）或让旧版报格式错误后重建。反过来，新版 fer 可**兼容加载 v3 dump**
（自动构建加速段，提示重建）。

## 版本更替约定

- 每次「大改前」把当时在用的 release 覆盖进本目录，并更新本表（来源 commit + dump 版本）。
- 验证新版本稳定后，若确认不再需要旧备份，可直接覆盖更新，不必保留多份。
