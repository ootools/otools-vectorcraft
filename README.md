# VectorCraft · OTools 插件

把 [storytold/vectorcraft](https://github.com/storytold/vectorcraft)（开源、纯 Rust 重写的 Illustrator 替代品）
移植为 OTools 插件：上游的 UI 原封不动编译到 WebAssembly，外壳把「文件从哪来、存到哪去」
换成 OTools 宿主的对话框与磁盘读写。

- 插件目录：`plugins/otools-vectorcraft`（在 [ootools/OTools](https://github.com/ootools/OTools) 主仓里）
- 上游源码：**不入库**，见 `upstream.pin` + `scripts/fetch-upstream.mjs`

## 上游版本

打包所用的上游提交。`upstream.pin` 是唯一真源，这张表用来回溯「哪个插件版本打的是哪份上游代码」，
每次发布都要在这里加一行。

| 插件版本 | 上游标签 | 上游提交 | 打包日期 |
| --- | --- | --- | --- |
| 0.1.4 | v0.13.1 | `904f0fe5227bd987d55319e89a38b4b8f046998c` | 2026-10-21 |
| 0.1.3 | — | `0a33cff3f6e2a1e9e672f4c3816f58cd8d3d032e` | — |

## 构建

```bash
unset RUSTC_WRAPPER                 # 它会把 cargo 指向旧 rustc
node scripts/fetch-upstream.mjs     # 拉取 pinned 的上游到 upstream/
node scripts/build-wasm.mjs         # trunk build --release → dist/
```

需要 rustc >= 1.95 与 wasm32-unknown-unknown 目标（egui 0.36 的要求）。

详细约定见 [AGENTS.md](./AGENTS.md)。
