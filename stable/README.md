# stable — 在役稳定版备份

> 目的：开发迭代期间保一个「已知可用」的 fer.exe，随时可回退。
> 当前 `$env:FER` 仍指向 `target-gnu\release\fer.exe`（开发版）；此目录为回退锚点。

## 当前备份

| 项 | 值 |
|---|---|
| 文件 | `stable\fer.exe` |
| SHA256 | `EA328CD80C31BCBE0694442FF126CE4A0AA1F5F7CCAA6CF5F8C111BFF912FB88` |
| 备份时间 | 2026-09-01（构建层优化轮完成后，commit `ae9a33c`） |
| 来源 commit | `ae9a33c`（bitmap 跳读 + 并行解析 + trigram 三遍构建） |
| dump 格式 | **FERIDX01 v5**（trigram 倒排段；兼容加载 v3/v4） |
| 已知性能 | 查询 serve 稳态子串 0-23ms/CJK 0-1ms；构建 7.9s/峰值 RSS 1.4GB |

## 回退方法

```powershell
Copy-Item D:\Kita-Tools\Coding\File-Engine-Rust\stable\fer.exe D:\Kita-Tools\Coding\File-Engine-Rust\target-gnu\release\fer.exe -Force
```

注意：**本备份能读 v3/v4/v5 三种 dump**（v3 自动内存重建加速段并提示 `fer upgrade`）。
若回退到更老的版本（v4 之前的 commit），v5 dump 无法加载，需重跑 `fer index`。

## 版本更替约定

- 每次「大改前」把当时在用的 release 覆盖进本目录，并更新本表（来源 commit + dump 版本）。
- 验证新版本稳定后，若确认不再需要旧备份，可直接覆盖更新，不必保留多份。
