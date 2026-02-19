# WezTerm Windows 手工 GUI 稳定性 QA 清单（wezterm-ysa.2）

## 目标

验证当前 Windows 稳定性修复在真实桌面交互中的表现，覆盖 CLI 自动化无法完全替代的场景：

1. 长时间稳定性（Parser EOF 是否消失）
2. 高输出下键盘输入流畅性（F09）
3. 高负载粘贴重试（F03）
4. 鼠标事件重试（F04）
5. CLI 通信稳定性
6. GUI 崩溃后的 CLI 可用性（F07 / WAIT_ABANDONED）
7. 输入死锁是否消失（F01）

---

## 测试前准备（统一）

### 1) 构建与版本确认

在仓库根目录执行：

```powershell
powershell -ExecutionPolicy Bypass -File .\build-wezterm.ps1
.\target\release\wezterm.exe -V
```

**通过标准**
- 构建成功，退出码为 `0`
- 版本命令返回版本号（非报错退出）

### 2) 日志目录与环境变量

```powershell
$ts = Get-Date -Format "yyyyMMdd-HHmmss"
$qaRoot = Join-Path $PWD "qa-manual-$ts"
New-Item -ItemType Directory -Force $qaRoot | Out-Null
New-Item -ItemType Directory -Force (Join-Path $qaRoot "logs") | Out-Null
New-Item -ItemType Directory -Force (Join-Path $qaRoot "screenshots") | Out-Null

$env:RUST_LOG = "info,mux=trace,wezterm_mux_server_impl=trace,wezterm_client=trace"
$env:WEZTERM_LOG = (Join-Path $qaRoot "logs\wezterm-gui.log")
```

**通过标准**
- `qa-manual-<timestamp>` 目录已创建
- `RUST_LOG`/`WEZTERM_LOG` 已设置

### 3) 启动测试实例

```powershell
Start-Process .\target\release\wezterm-gui.exe
Start-Sleep -Seconds 3
.\target\release\wezterm.exe cli list | Tee-Object (Join-Path $qaRoot "logs\cli-list-initial.txt")
```

**通过标准**
- GUI 正常启动
- `cli list` 返回至少 1 条 pane 记录

---

## Case 1：长时间稳定性（Parser EOF）

### 操作步骤

1. 在 GUI pane 中执行持续输出命令（任选）：
   - `for /L %i in (1,1,30000) do @echo %i`
   - 或 `ping -t 127.0.0.1`
2. 保持运行至少 30 分钟（建议 60 分钟）
3. 每 10 分钟执行一次：
   ```powershell
   .\target\release\wezterm.exe cli list | Tee-Object -Append (Join-Path $qaRoot "logs\cli-list-during.txt")
   ```
4. 测试结束后保存日志副本。

### 通过标准

- 无“周期性每 5 分钟 EOF + 恢复风暴”现象
- GUI 无冻结、无明显卡顿
- `cli list` 持续可用

### 失败判定

- 日志中出现高频重复 EOF/recovery（接近周期性）
- GUI 输入或输出长时间中断

---

## Case 2：高输出负载下键盘输入流畅性（F09）

### 操作步骤

1. 启动高输出：
   - `for /L %i in (1,1,200000) do @echo load-%i`
2. 输出期间持续按键输入（如 `abcdef`、方向键、Ctrl+C）
3. 观察字符回显延迟与交互抖动
4. 记录体感延迟（建议主观分档：<100ms / 100-300ms / >300ms）

### 通过标准

- 输入可持续被处理，无明显“卡住数秒后突发回显”
- 无长时间饥饿（输入完全无响应）

### 失败判定

- 出现秒级输入阻塞或明显堆积喷发

---

## Case 3：高负载粘贴可靠性（F03）

### 操作步骤

1. 准备大文本（建议 2K~10K 行）
2. 在高输出期间执行多次粘贴（Ctrl+V / 右键粘贴）
3. 在 shell 中统计行数或关键字，确认未丢行

### 通过标准

- 粘贴内容最终完整到达 pane
- 无粘贴触发 GUI 卡死
- 日志可有 WouldBlock 重试，但不能导致丢内容

### 失败判定

- 粘贴内容部分丢失或顺序严重错乱
- 粘贴后 GUI 不可交互

---

## Case 4：鼠标操作（vim / tmux）稳定性（F04）

### 操作步骤

1. 在 pane 内启动 `vim`（或 `tmux`）
2. 高频进行：
   - 光标移动、选中、滚轮滚动
   - 快速点击/拖拽
3. 同时保持中高输出（另开 pane）

### 通过标准

- 鼠标事件持续生效，无随机“吞点击”
- 滚轮与拖拽行为稳定

### 失败判定

- 鼠标输入经常丢失且无恢复
- 出现交互性卡死

---

## Case 5：CLI 通信稳定性

### 操作步骤

连续执行（建议 100 次）：

```powershell
for($i=1; $i -le 100; $i++){
  .\target\release\wezterm.exe cli list | Out-Null
}
```

再做一次 pane 操作链路验证：

```powershell
$pane = .\target\release\wezterm.exe cli spawn --new-window -- cmd.exe
$pane = $pane.Trim()
.\target\release\wezterm.exe cli send-text --pane-id $pane --no-paste "echo qa-cli-path`r"
Start-Sleep -Milliseconds 500
.\target\release\wezterm.exe cli get-text --pane-id $pane | Select-String "qa-cli-path"
.\target\release\wezterm.exe cli kill-pane --pane-id $pane
```

### 通过标准

- `cli list` 持续成功，无间歇性连接失败
- `spawn/send-text/get-text/kill-pane` 全链路成功

---

## Case 6：GUI 崩溃后的 CLI 行为（F07 / WAIT_ABANDONED）

### 操作步骤

1. 保持至少 1 个 pane 活动
2. 强制终止 GUI 进程（任务管理器或命令）：
   ```powershell
   Stop-Process -Name wezterm-gui -Force
   ```
3. 立即执行：
   ```powershell
   .\target\release\wezterm.exe cli list | Tee-Object (Join-Path $qaRoot "logs\cli-list-after-gui-kill.txt")
   ```
4. 重新启动 GUI，再次执行 `cli list`

### 通过标准

- 不出现永久锁死（mutex abandoned 后应可恢复路径处理）
- 重启 GUI 后 CLI 恢复可用

### 失败判定

- CLI 长期不可用，且重启后仍失败

---

## Case 7：输入死锁回归（F01）

### 操作步骤

1. 制造“进程等输入、几乎无输出”场景（如等待命令输入提示）
2. 快速连续输入并交替粘贴
3. 同时切换焦点、切换 tab、最小化/恢复窗口
4. 观察是否出现“输入永远不再发送”的死锁

### 通过标准

- 输入最多短暂延迟后可恢复发送
- 不出现永久停滞

### 失败判定

- 输入队列长期不再出队，必须重启进程才恢复

---

## 结果记录模板（每个 Case 一条）

建议在 `bd comments add wezterm-ysa.2` 追加如下结构：

```text
Case: <编号与名称>
Result: PASS / FAIL
Start-End: <时间段>
Evidence:
- 日志文件: <路径>
- 截图: <路径>
- 命令输出: <路径>
Notes:
- <现象摘要>
```

---

## 出口条件（可关单标准）

`wezterm-ysa.2` 可关闭前提：

1. Case 1~7 全部 PASS，或
2. FAIL 项已创建新的 `bd` 缺陷任务并附复现证据

主任务 `wezterm-ysa` 可关闭前提：

- 自动化验证通过 + 本手工 QA 完成并有证据链。
