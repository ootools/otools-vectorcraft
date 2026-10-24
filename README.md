# VectorCraft · OTools 插件

把 [storytold/vectorcraft](https://github.com/storytold/vectorcraft)（开源、纯 Rust 重写的 Illustrator 替代品）
移植为 OTools 插件：上游的 UI 原封不动编译到 WebAssembly，外壳把「文件从哪来、存到哪去」
换成 OTools 宿主的对话框与磁盘读写。

- 插件目录：`plugins/otools-vectorcraft`（在 [ootools/OTools](https://github.com/ootools/OTools) 主仓里）
- 上游源码：**不入库**，见 `upstream.pin` + `scripts/fetch-upstream.mjs`

## MCP

插件通过宿主 MCP 网关暴露上游 `vectorcraft-mcp` 的 32 个工具、3 个资源、4 个资源模板
和 5 个 prompt：

| 能力 | 内容 |
| --- | --- |
| tools | 32 个（`mcp.expose` 是默认勾选种子） |
| resources | `vectorcraft://document`、`vectorcraft://document/json`、`vectorcraft://commands` |
| resourceTemplates | `object/{id}`、`command/{id}`、`effect/{id}`、`swatch/{name}` |
| prompts | `poster`、`icon-set`、`recolor`、`trace-and-style`、`export-set` |

资源模板的 `{id}` 解析、percent-decoding（`Brand%2040%25` 这种）和 prompt 的默认值替换
全部在上游 `Server::handle` 里完成。适配层用 `Proxy`（`Arc<Mutex<Headless>>`）让上游
`Server` 能拿到 `Box<dyn Backend>`，自己不留任何 URI 或模板的副本。

```bash
cargo test --release --manifest-path native/Cargo.toml
cargo build --release --manifest-path native/Cargo.toml
node scripts/smoke-mcp.mjs ..
```

## 上游版本

打包所用的上游提交。`upstream.pin` 是唯一真源，这张表用来回溯「哪个插件版本打的是哪份上游代码」，
每次发布都要在这里加一行。

| 插件版本 | 上游标签 | 上游提交 | 打包日期 |
| --- | --- | --- | --- |
| 0.1.5 | v0.13.1 | `904f0fe5227bd987d55319e89a38b4b8f046998c` | 2026-10-24 |
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
