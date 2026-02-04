# HiFi Replayer - 技术文档

> 本文档面向 AI Agent 和开发者，说明项目架构、设计决策和开发注意事项。

## 项目定位

**极致音质的音频播放器**，专注于：
- 时序绝对稳定（低 jitter）
- Bit-perfect 数据传输
- 最短信号路径

**不是通用播放器**，不追求功能丰富，而是在 macOS + CoreAudio 约束下将软件层优化到极限。

---

## 架构概览

```
┌─────────────────────────────────────────────────────────────────┐
│                         Engine (引擎层)                          │
│  - 整合解码、缓冲、输出                                            │
│  - 管理播放状态 (Stopped/Playing/Paused/Buffering)                │
└─────────────────────────────────────────────────────────────────┘
        │                                      │
        ▼                                      ▼
┌───────────────────┐              ┌───────────────────────────────┐
│   Decoder Thread  │              │    CoreAudio IO Thread        │
│   (解码线程)       │              │    (音频输出回调)              │
│                   │              │                               │
│ - symphonia 解码   │  Lock-free   │ - THREAD_TIME_CONSTRAINT      │
│ - i32 左对齐转换   │ ──────────→  │ - 零拷贝/格式转换              │
│ - SIMD 加速       │  Ring Buffer │ - TPDF Dither (Float32)       │
│                   │   (SPSC)     │ - 实时优先级                   │
└───────────────────┘              └───────────────────────────────┘
```

### 关键设计原则

1. **解码和输出完全解耦** - 通过 lock-free ring buffer 连接，互不阻塞
2. **IO 回调中禁止**：内存分配、加锁、系统调用、日志
3. **数据格式统一** - 全程使用 i32 左对齐，避免多次转换
4. **SRC 由 CoreAudio 处理** - 当源采样率与设备不匹配时，使用 CoreAudio 内置 SRC

---

## 模块结构

```
src/
├── lib.rs              # 库入口，导出公共 API
├── main.rs             # CLI 入口
├── audio/
│   ├── mod.rs          # 音频模块导出
│   ├── output.rs       # CoreAudio 输出 (HALOutput/DefaultOutput)
│   ├── ring_buffer.rs  # Lock-free SPSC 环形缓冲区
│   ├── stats.rs        # 实时统计（降频采样）
│   ├── format.rs       # 音频格式定义
│   └── dither.rs       # TPDF 抖动实现
├── decode/
│   ├── mod.rs          # 解码模块导出
│   └── decoder.rs      # symphonia 解码器封装
└── engine/
    └── mod.rs          # 播放引擎（状态管理、线程协调）
```

---

## 核心组件详解

### 1. Ring Buffer (`audio/ring_buffer.rs`)

**职责**：解码线程和 IO 线程之间的数据传递

**关键特性**：
- **SPSC (Single-Producer Single-Consumer)** - 无需复杂同步
- **Wait-free** - 读写操作保证常数时间完成
- **CacheLine 对齐** - `#[repr(align(64))]` 避免 false sharing
- **mlock 锁定** - 防止 page fault 导致时序抖动
- **2^n 容量** - 位与代替取模，快速索引计算

```rust
// 核心数据结构
pub struct RingBuffer<T: Copy + Default> {
    buffer: Box<[UnsafeCell<T>]>,
    capacity: usize,
    mask: usize,  // capacity - 1，用于快速取模
    write_pos: CacheLine<AtomicUsize>,  // 生产者位置
    read_pos: CacheLine<AtomicUsize>,   // 消费者位置
    memory_locked: AtomicBool,
}
```

**使用方式**：
- 解码线程调用 `write()` 写入样本
- IO 回调调用 `read()` 读取样本
- 两者可完全并发，无锁

### 2. 音频输出 (`audio/output.rs`)

**职责**：CoreAudio 设备管理和音频输出

**输出模式优先级**：
1. **Physical Format (Int32)** - 直接硬件访问，bit-perfect
2. **ASBD Integer (Int32/Int24)** - 通过 AudioUnit 整数输出
3. **Float32 + TPDF Dither** - 回退路径（蓝牙、DefaultOutput）

**关键技术**：
- **HALOutput vs DefaultOutput**
  - HALOutput: 绕过系统混音器，直接访问 USB DAC
  - DefaultOutput: 通过系统混音器，用于蓝牙等设备
- **Hog Mode**: 独占设备，防止其他应用干扰
- **采样率智能选择**: 精确匹配 > 整数分频 > 最近值
- **IO 线程实时调度**: `THREAD_TIME_CONSTRAINT_POLICY`
- **CoreAudio SRC**: 当源采样率与设备不匹配时，由 CoreAudio 自动处理采样率转换

**回调函数 (`render_callback`)**：
```rust
// 首次调用设置实时线程策略（只执行一次）
if ctx.thread_policy_set.compare_exchange(...).is_ok() {
    ctx.set_realtime_thread_policy();
}

// 根据输出模式选择路径
match ctx.output_mode {
    Int32 => // 零拷贝：ring_buffer → 输出缓冲区
    Int24 => // 转换：ring_buffer → sample_buffer → 24-bit packed
    Float32 => // 转换 + dither：ring_buffer → sample_buffer → f32 + TPDF
}
```

### 3. 解码器 (`decode/decoder.rs`)

**职责**：音频文件解码，输出 i32 左对齐样本

**支持格式**：FLAC, WAV, AIFF, MP3（通过 symphonia）

**整数直通路径**：
- 对于 PCM 整数源（16/24/32-bit），直接转换到 i32
- 避免 f64 中间表示，保持 bit-perfect

**SIMD 优化**（ARM NEON，Apple Silicon）：
- 立体声 i16 → i32：`vmovl_s16` + `vshlq_n_s32`
- 立体声 i24 → i32：`vld1q_s32` + `vshlq_n_s32`
- 每次处理 4 帧（8 样本），剩余标量处理

**样本格式（左对齐到 i32 高位）**：
```
16-bit: [S S S S S S S S S S S S S S S S 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0]
        |<------ 16-bit sample ------>|  |<-------- zero padding ------->|
        bit 31                      bit 16                              bit 0

24-bit: [S S S S S S S S S S S S S S S S S S S S S S S S 0 0 0 0 0 0 0 0]
        |<------------ 24-bit sample ------------->|    |<-- padding -->|
        bit 31                                    bit 8                bit 0

32-bit: [S S S S S S S S S S S S S S S S S S S S S S S S S S S S S S S S]
        |<---------------------- 32-bit sample ----------------------->|
```

### 4. 播放引擎 (`engine/mod.rs`)

**职责**：整合各模块，管理播放状态

**状态机**：
```
Stopped ──play()──→ Buffering ──prebuffer完成──→ Playing
    ↑                   │                          │
    │                   └──toggle_pause()──────────┤
    │                                              ↓
    └──────────────stop()──────────────────────Paused
```

**线程管理**：
- 解码线程：`SCHED_RR:47` 或 `nice -10`（非 root 回退）
- IO 线程：由 CoreAudio 管理，首次回调时设置 `THREAD_TIME_CONSTRAINT_POLICY`

**暂停机制**：
- 使用 `Condvar` 实现零延迟唤醒
- 解码线程在暂停时等待，不消耗 CPU

**SRC 处理**：
- 当源采样率与设备采样率不匹配时，由 CoreAudio 内置 SRC 处理
- 解码线程直接写入源采样率数据到 ring buffer
- CoreAudio 自动转换到设备采样率

### 5. 统计模块 (`audio/stats.rs`)

**职责**：实时采集性能数据，最小化回调开销

**降频采样**：
- 每 16 次回调才采样一次
- 避免频繁原子操作影响性能

**采集数据**：
- 回调间隔（使用 `mach_timebase_info` 转换）
- 缓冲区水位
- Underrun 次数
- 已播放样本数

---

## 性能优化清单

### 时序稳定性优化

| 优化项 | 实现位置 | 说明 |
|--------|----------|------|
| Lock-free SPSC Ring Buffer | `ring_buffer.rs` | 生产者/消费者完全无锁，wait-free |
| CacheLine 对齐 | `ring_buffer.rs` | `#[repr(align(64))]` 避免 false sharing |
| mlock 内存锁定 | `ring_buffer.rs`, `output.rs` | 防止 page fault 导致时序抖动 |
| IO 线程实时调度 | `output.rs` | `THREAD_TIME_CONSTRAINT_POLICY` |
| 解码线程优先级 | `engine/mod.rs` | `SCHED_RR:47` 或 `nice -10` 回退 |
| 统计降频采样 | `stats.rs` | 每 16 次回调才采样，减少原子操作开销 |
| Condvar 暂停机制 | `engine/mod.rs` | 零延迟唤醒，避免轮询 |
| 硬件时间戳 | `stats.rs` | 使用 `AudioTimeStamp.host_time` 提升时序精度 |

### Bit-Perfect 信号路径优化

| 优化项 | 实现位置 | 说明 |
|--------|----------|------|
| 物理格式输出 | `output.rs` | 直接设置 `kAudioStreamPropertyPhysicalFormat`，绕过系统格式转换 |
| 零拷贝输出 (Int32) | `output.rs` | ring buffer → 输出缓冲区直接读取 |
| 整数直通路径 | `decoder.rs` | PCM 整数源直接转 i32，避免 f64 中间表示 |
| i32 左对齐统一格式 | 全局 | 全程 i32 左对齐，避免多次转换 |
| Hog Mode | `output.rs` | 独占设备，防止系统混音器干扰 |
| 采样率智能选择 | `output.rs` | 精确匹配 > 整数分频 > 最近值 |
| CoreAudio SRC | 系统内置 | 高质量采样率转换，由 CoreAudio 处理 |

### 性能优化

| 优化项 | 实现位置 | 说明 |
|--------|----------|------|
| ARM NEON SIMD | `decoder.rs` | 立体声 i16/i24 → i32 向量化转换 |
| 2^n 容量位运算 | `ring_buffer.rs` | `mask & pos` 代替取模 |
| 内存预分配 | 全局 | 所有缓冲区初始化时分配，回调中无 alloc |
| TPDF Dither | `dither.rs` | xorshift32 PRNG，realtime-safe |

### 输出模式优先级

```
Physical Format (Int32) → ASBD Integer → Float32 + Dither
       最优                  次优           回退
    bit-perfect          bit-perfect      有量化
```

### 可选改进（边际收益）

| 优化项 | 预期收益 | 说明 |
|--------|----------|------|
| i32/多声道 SIMD | 低 | 当前只有立体声 i16/i24 有 NEON |
| Ring Buffer 批量写入 | 低 | memcpy 替代循环，需处理 wrap |
| Prefetch 指令 | 微小 | 现代 CPU 预取已很智能 |
| 静音检测跳过 dither | 微小 | Float32 回退路径下消除 ~-144dB 噪声 |

---

## 开发注意事项

### 实时回调中的禁忌

```rust
// ❌ 绝对禁止
fn render_callback(...) {
    let vec = Vec::new();           // 内存分配
    mutex.lock();                    // 加锁
    log::info!("...");              // 日志（可能阻塞）
    std::fs::read(...);             // 文件 I/O
    thread::sleep(...);             // 睡眠
}

// ✅ 正确做法
fn render_callback(...) {
    ring_buffer.read(&mut output);  // wait-free 原子操作
    stats.record_underrun();        // 原子计数
    // 所有数据在回调外预分配
}
```

### 原子操作 Ordering 选择

```rust
// 生产者写入后需要让消费者可见
write_pos.store(new_pos, Ordering::Release);

// 消费者读取前需要看到最新写入
let write = write_pos.load(Ordering::Acquire);

// 内部状态更新，无需跨线程同步
callback_count.fetch_add(1, Ordering::Relaxed);
```

### 添加新功能的检查清单

1. **是否在 IO 回调路径上？**
   - 是 → 必须 wait-free，无分配，无系统调用
   - 否 → 可以使用标准库功能

2. **是否需要跨线程共享？**
   - 是 → 使用原子类型或通过 ring buffer 传递
   - 否 → 普通变量即可

3. **是否影响音频路径？**
   - 是 → 需要性能测试，确保不增加 jitter
   - 否 → 正常开发

---

## 扩展方向

### 已规划（readme.md）

- [ ] A/B 对比切换
- [ ] 播放列表支持
- [ ] DSD 支持

### 架构建议

**添加播放列表**：
```
Playlist Manager (主线程)
    │
    ├── 预加载下一曲 metadata
    ├── 无缝切换（gapless）：预解码 + 交叉淡入淡出
    └── 状态持久化
```

**添加 A/B 对比**：
```
Source A ──→ ┐
             ├─→ Switch ──→ Ring Buffer ──→ Output
Source B ──→ ┘
             ↑
         原子切换，延迟同步
```

---

## 测试

```bash
# 编译
cargo build --release

# 运行（需要音频文件）
./target/release/hifi-replayer music.flac

# 运行测试
cargo test

# 运行带设备的测试（需要音频设备）
cargo test -- --ignored
```

---

## 依赖

| Crate | 用途 |
|-------|------|
| symphonia | 音频解码（FLAC/WAV/AIFF/MP3） |
| clap | 命令行参数解析 |
| log + env_logger | 非阻塞日志 |
| libc | 系统调用（mlock, pthread） |
| crossbeam-utils | 并发工具 |

---

## 联系

如有问题，请参考：
- `readme.md` - 用户文档
- `src/` 各模块注释 - 实现细节
- 本文档 - 架构和开发指南
