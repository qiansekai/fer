# stable — 在役稳定版备份

> 目的：开发迭代期间保一个「已知可用」的 fer.exe，随时可回退。
> 当前 `$env:FER` 仍指向 `target-gnu\release\fer.exe`（开发版）；此目录为回退锚点。

## 当前备份

| 项 | 值 |
|---|---|
| 文件 | `stable\fer.exe` |
| SHA256 | `BE3622429C46B593E1824F9055047F17190B6B4D87FCD1242B1F70AACD4153B7` |
| 备份时间 | 2026-09-01（trigram v5 轮完成后） |
| 来源 commit | `6c27bd1` 之后（v5 trigram + `fer upgrade`，正式 commit 见 git log） |
| dump 格式 | **FERIDX01 v5**（trigram 倒排段；兼容加载 v3/v4） |
| 已知性能 | serve 稳态：子串 0-23ms、CJK 2 字 0-1ms、路径子串 90-135ms、正则 4ms、glob 9-134ms |

## 回退方法

```powershell
Copy-Item D:\Kita-Tools\Coding\File-Engine-Rust\stable\fer.exe D:\Kita-Tools\Coding\File-Engine-Rust\target-gnu\release\fer.exe -Force
```

注意：**本备份能读 v3/v4/v5 三种 dump**（v3 自动内存重建加速段并提示 `fer upgrade`）。
若回退到更老的版本（v4 之前的 commit），v5 dump 无法加载，需重跑 `fer index`。

## 版本更替约定

- 每次「大改前」把当时在用的 release 覆盖进本目录，并更新本表（来源 commit + dump 版本）。
- 验证新版本稳定后，若确认不再需要旧备份，可直接覆盖更新，不必保留多份。
