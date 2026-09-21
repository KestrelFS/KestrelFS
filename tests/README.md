# KestrelFS 测试脚本约定

自 Step 55 验收起，**所有手工/vng/门控测试脚本与 C helper 放在本目录**，不再新增到仓库根目录。

## 布局

| 路径 | 用途 |
|---|---|
| `tests/test-stepNN-*.sh` | vng guest 脚本、宿主编排器、Redis/TLS 门控 |
| `tests/test-stepNN-*.c` | 对应 C helper（由脚本在 guest 内 `cc`） |
| `tests/_repo_root.sh` | 公共引导：把 cwd 切到仓库根，便于使用 `daemon/`、`kestrelfs/` |
| `tests/test-persistence.sh` / `test-vm-*.sh` / `test-virtme.sh` | 历史/VM 辅助脚本 |

`daemon/` 内 `cargo test` 仍留在 Rust 树内，不迁到这里。
`tools/`、`vm-test/` 保持原位（构建产物/工具，不是 step 回归脚本）。

## 运行方式

始终在**仓库根**调用（脚本会自行 `cd` 到根）：

```bash
# guest 内回归（示例）
vng --run --network user --rwdir "$PWD" --cwd "$PWD" \
  --exec ./tests/test-step55-dtype-mknod-vng.sh

# 需要 Redis/S3 的宿主编排器
REDIS_URL=... S3_ENDPOINT=... ./tests/test-step52-dist-vng.sh
```

## 新脚本规则（站立）

1. **只写入 `tests/`**；禁止在仓库根新增 `test-*`。
2. 每个 `.sh` 在 `set -euo pipefail` 后 source `_repo_root.sh`。
3. 编译 helper 使用 `tests/foo.c`（相对仓库根）。
4. 互相调用使用 `./tests/other.sh`。
5. 内核/mount/cache 仍只允许在 vng guest + loop；daemon 日志写 `"$data_dir/daemon.log"`。
6. 新 step 的 PASS 标记继续用 `STEPNN_*_PASS` 前缀。
