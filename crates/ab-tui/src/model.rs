//! Read model 與 selection model（tui-design.md §2／§4）。
//!
//! 資料來源分兩層、兩種節奏（§4）：
//! - 磁碟 task-plane＋registry：每 500ms 重讀（權威）
//! - tmux liveness：每 2s 一輪（只補位置與死活；查不到＝unknown，不覆寫權威）
//!
//! 純資料轉換全部放這裡（不碰 terminal），單元測試不經 render。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ab_core::paths::Paths;
use ab_core::registry::{self, AgentSnapshot};
use ab_core::task::{self, InFlight};
use ab_core::tmux::TmuxClient;

/// TASKS 面板一次載入的任務上限。`tasks/` 會長大而 dashboard 每 500ms 掃一
/// 輪：截斷（在讀檔前）是這條路徑唯一的成本上限。200 列遠超過一畫面高度，
/// 人要看更舊的請走 `list`／`gc` 那條 CLI 路徑。
pub const RECENT_LIMIT: usize = 200;

/// 磁碟 read model 的一輪快照。
pub struct Model {
    /// WORKERS 欄的 **lineage 分組**（P4.7 切片 B：唯一邏輯軸）。
    ///
    /// ORIGINS 面板在此退場：origin＝spawn 當下的 window id，是**物理位置**
    /// 不是邏輯 principal，relay 鏈一接棒就斷裂（§11 根因判定）。物理位置沒有
    /// 消失，它降到 DETAIL 當證據列——那裡它是「這個 worker 現在坐在哪」，
    /// 是事實；當成歸屬軸才是謊。
    pub groups: Vec<Group>,
    pub workers: Vec<AgentSnapshot>,
    pub tasks: Vec<InFlight>,
    /// TASKS 面板用：近期任務（**含終態**），id 反序。與 `tasks` 分開存放
    /// ——WORKERS 欄要的是 in-flight，TASKS 欄要的是「有東西可讀」的全集。
    pub recent: Vec<InFlight>,
    /// `recent` 是否因為 `RECENT_LIMIT` 而**還有更舊的沒載入**。畫面上的
    /// `N/total` 要據此標示，否則人會以為看到的就是全部（P4.6 切片 C）。
    pub recent_truncated: bool,
}

impl Model {
    pub fn load(paths: &Paths) -> Self {
        let workers = registry::snapshot(paths);
        let tasks = task::in_flight(paths);
        let recent = task::recent_tasks(paths, RECENT_LIMIT);
        let groups = group_by_lineage(&workers);
        Model {
            groups,
            workers,
            tasks,
            recent: recent.tasks,
            recent_truncated: recent.truncated,
        }
    }

    /// worker 名 → `workers` 索引（TASKS 欄的列要回頭找所屬 worker）。
    pub fn worker_idx(&self, name: &str) -> Option<usize> {
        self.workers.iter().position(|w| w.name == name)
    }
}

/// worker 的 origin 標籤：spawned 且有 owner 欄→其字面值；manual→`-`；
/// spawned 但 owner 缺失（或 registry 損壞）→`?`。
///
/// 名字是 origin 不是 owner：registry 的 `owner` 欄是 **spawn 當下的 window
/// id**，是物理位置而非邏輯 principal（P4.6 §11 根因判定）。
pub fn origin_label(w: &AgentSnapshot) -> String {
    if w.corrupt {
        "?".to_string()
    } else if !w.spawned {
        "-".to_string()
    } else if w.owner.is_empty() {
        "?".to_string()
    } else {
        w.owner.clone()
    }
}

/// 一列 worker 的 lineage 歸屬（P4.7 切片 B：**唯一邏輯軸**）。
///
/// 契約逐條（§9 P4.7）：新式列依 `lineage_root` 歸組；legacy 列（兩欄缺席）
/// 只有在**自身 `spawn_tag` 等於某組的 root key** 時才歸組（證據在子代側，
/// legacy 永不 backfill）；人工註冊沒有世代可言；兩欄不可信者一律 invalid。
/// **MUST NOT 由 task 的 `from`／`to` 推導**——那是訊息往來，不是出身。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Affiliation {
    /// 屬於某條 lineage，值是該組的 generation key
    Lineage(String),
    /// 有 spawn_tag，但兩欄缺席且自身不是任何一組的根
    Legacy,
    /// 人工註冊（`register`）：沒有 spawn_tag，也沒有世代
    Manual,
    /// 兩欄不可信：非 generation key（含 `Some("")` 的 invalid 標記）、
    /// 自己是自己的 parent、與 parent 分屬不同 lineage、registry 損壞
    Invalid,
}

/// WORKERS 欄的一組。lineage 組以 generation key 為身分；四型 standalone
/// 共用一個尾段（每一列自帶 `Affiliation` 標記，見 `worker_affiliation`）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GroupKey {
    Lineage(String),
    /// legacy／manual／invalid 的收容段——**它們是 standalone，不是一個組**：
    /// 這一段只是版面上的落點，成員之間沒有任何歸屬關係
    Standalone,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Group {
    pub key: GroupKey,
    /// 成員在 `Model::workers` 的索引，維持 snapshot 序（檔名字典序）
    pub members: Vec<usize>,
}

/// 這一列的 `lineage_root` 說得出口嗎——說得出口才拿它當組別。
fn valid_root(w: &AgentSnapshot) -> Option<&str> {
    w.lineage_root
        .as_deref()
        .filter(|v| ab_core::spawn::is_generation_key(v))
}

/// **一列的形狀**（不依賴其他列的部分）。
///
/// 拆出來是因為歸屬判定有兩個層次：形狀是這一列自己說得通不通，歸組才需要
/// 看別人（legacy 要不要被吸附進某條 lineage，取決於有沒有子代以它為根）。
/// 混在一起寫的後果是 **invalid 列的 root 會汙染 `roots`**——一列自相矛盾的
/// registry 就能無中生有一個組別（切片 B1 修正輪 G1 的反例）。
enum Shape {
    /// 通過形狀驗證的新式 lineage 列，值＝該列的組別 key
    Lineage(String),
    /// 有 spawn_tag、兩欄皆缺席
    Legacy,
    Manual,
    Invalid,
}

/// 逐列判形狀。**registry 兩欄是 worker 可寫面**，所以每一種缺漏／矛盾都要
/// 有明確歸屬，不能靠「大概是舊版寫的」放行。
fn row_shape(w: &AgentSnapshot, by_tag: &HashMap<&str, &AgentSnapshot>) -> Shape {
    if w.corrupt {
        return Shape::Invalid;
    }
    if w.spawn_tag.is_empty() {
        // 人工註冊（`register`）：沒有世代，也**不該**有 spawned 或 lineage 欄。
        // 三者有任何一項對不上，這一列在說一件不可能的事——不是 manual，是
        // invalid（`spawned:true` 卻沒有 spawn_tag 尤其可疑）
        return if !w.spawned && w.lineage_root.is_none() && w.parent_agent.is_none() {
            Shape::Manual
        } else {
            Shape::Invalid
        };
    }
    match (&w.lineage_root, w.parent_agent.as_deref()) {
        // legacy：**兩欄皆缺席**（欄位永不 backfill）
        (None, None) => Shape::Legacy,
        // parent-only：有 parent 卻說不出自己屬於哪一條 lineage。半殘的形狀
        // 不得被當成 legacy 放行——它宣稱了一段血緣，卻拒絕說是哪一段
        (None, Some(_)) => Shape::Invalid,
        (Some(_), parent) => {
            let Some(root) = valid_root(w) else {
                // `Some("")`（型別錯誤的 invalid 標記）與任何不合文法的值
                return Shape::Invalid;
            };
            match parent {
                Some(p) => {
                    if !ab_core::spawn::is_generation_key(p) {
                        return Shape::Invalid;
                    }
                    // 自己是自己的 parent：這一列在說一件不可能的事
                    if p == w.spawn_tag {
                        return Shape::Invalid;
                    }
                    // parent 在場但分屬不同 lineage：兩欄互相矛盾，不猜哪一邊對
                    if let Some(pw) = by_tag.get(p)
                        && let Some(proot) = valid_root(pw)
                        && proot != root
                    {
                        return Shape::Invalid;
                    }
                }
                // 沒有 parent 的新式列只有一種說得通的形狀：**它自己就是根**。
                // root 指向別人卻不說自己是誰的子代，等於憑空認領一個組別
                None => {
                    if root != w.spawn_tag {
                        return Shape::Invalid;
                    }
                }
            }
            Shape::Lineage(root.to_string())
        }
    }
}

/// 單列的歸屬判定（B6 防護中**歸組會用到的那部分**：非 generation key／
/// 自指／跨 lineage 不一致／半殘形狀 → invalid → standalone，不參與任何組）。
///
/// `by_tag` 是「spawn_tag → 該列」的索引，`roots` 是本輪**已通過形狀驗證的**
/// lineage 列所宣告的組別 key。兩者都由呼叫端在同一份快照上建好——registry
/// 兩欄是 worker 可寫面，**不得假設它們構成一棵樹**。
pub fn worker_affiliation(
    w: &AgentSnapshot,
    roots: &std::collections::HashSet<String>,
    by_tag: &HashMap<&str, &AgentSnapshot>,
) -> Affiliation {
    match row_shape(w, by_tag) {
        Shape::Lineage(root) => Affiliation::Lineage(root),
        // legacy 的唯一歸組理由：**別人**以它的 spawn_tag 為根（子代側的證據）
        Shape::Legacy => {
            if roots.contains(&w.spawn_tag) {
                Affiliation::Lineage(w.spawn_tag.clone())
            } else {
                Affiliation::Legacy
            }
        }
        Shape::Manual => Affiliation::Manual,
        Shape::Invalid => Affiliation::Invalid,
    }
}

/// 把一份 registry 快照攤成 WORKERS 欄的分組（P4.7 切片 B）。
///
/// 三趟：`by_tag`（不依賴任何判定）→ 逐列形狀＋收 `roots`（**只收形狀合法的
/// lineage 列**）→ legacy 吸附。第二、三趟不能合併：legacy 要不要歸組取決於
/// 全部子代看完之後的 `roots`。
pub fn group_by_lineage(workers: &[AgentSnapshot]) -> Vec<Group> {
    let by_tag: HashMap<&str, &AgentSnapshot> = workers
        .iter()
        .filter(|w| !w.corrupt && !w.spawn_tag.is_empty())
        .map(|w| (w.spawn_tag.as_str(), w))
        .collect();
    // 第一趟：所有**說得出口的**組別 key。invalid 列的 root 不進來——否則
    // 一列自相矛盾的 registry 就能讓一個 legacy 列被吸進不存在的組
    let roots: std::collections::HashSet<String> = workers
        .iter()
        .filter_map(|w| match row_shape(w, &by_tag) {
            Shape::Lineage(r) => Some(r),
            _ => None,
        })
        .collect();

    // 第二趟：逐列判歸屬。lineage 組依 key 字典序（畫面每輪都要穩定），
    // standalone 段恆在最後
    let mut lineage: std::collections::BTreeMap<String, Vec<usize>> = Default::default();
    let mut standalone: Vec<usize> = Vec::new();
    for (i, w) in workers.iter().enumerate() {
        match worker_affiliation(w, &roots, &by_tag) {
            Affiliation::Lineage(k) => lineage.entry(k).or_default().push(i),
            Affiliation::Legacy | Affiliation::Manual | Affiliation::Invalid => standalone.push(i),
        }
    }
    let mut out: Vec<Group> = lineage
        .into_iter()
        .map(|(k, members)| Group {
            key: GroupKey::Lineage(k),
            members,
        })
        .collect();
    if !standalone.is_empty() {
        out.push(Group {
            key: GroupKey::Standalone,
            members: standalone,
        });
    }
    out
}

/// 一組的畫面標籤。
///
/// **display-only 的 tag 剖析**（契約允許的那條界線）：禁止的是拿 name 做
/// **歸組／lookup**——歸組全程只比對 generation key 全串，這裡剖析出來的
/// 字串一個字都不會回到判定路徑上，只用來讓人讀得懂畫面。
///
/// 根在場（有一列的 `spawn_tag` 等於組 key）→ 用它的 agent 名；不在場 →
/// tag 的 name 段＋世代短碼＋`†`（墓碑）。
pub fn group_label(model: &Model, g: &Group) -> String {
    let GroupKey::Lineage(key) = &g.key else {
        return "(standalone)".to_string();
    };
    if let Some(w) = model
        .workers
        .iter()
        .find(|w| !w.corrupt && w.spawn_tag == *key)
    {
        return format!("lineage {}", w.name);
    }
    match tag_display_parts(key) {
        Some((name, code)) => format!("lineage {name}\u{2020} ({code})"),
        // 連剖析都失敗：只說得出「這個 key 存在」，不編故事
        None => "lineage ?\u{2020}".to_string(),
    }
}

/// canonical generation key → `(name 段, 世代短碼)`，**display only**。
///
/// 短碼取 12 位 hex 的前 4 位：同名 respawn 在畫面上要分得出是哪一代，而
/// 完整 tag 塞不進一欄寬。呼叫端只在已通過文法驗證的 key 上用它。
pub fn tag_display_parts(key: &str) -> Option<(String, String)> {
    let rest = key.strip_prefix("AGENT_BRIDGE_SPAWN_TAG=ab-spawn-")?;
    let (head, hex) = rest.rsplit_once('-')?;
    let (name, _pid) = head.rsplit_once('-')?;
    Some((name.to_string(), hex.get(..4)?.to_string()))
}

/// 兩軸資料的**年紀**（P4.6 切片 C）：距離上一次可信的更新過了多久。
///
/// 為什麼要有這個型別：畫面上的死活與 blocker 是快照，快照會老。背景 worker
/// 卡住時，舊資料若一直原樣掛在畫面上，人看到的是「一切正常」——而正常的其實
/// 只有那份三十秒前的記憶。逾門檻就**降級為 unknown**：unknown 是誠實的，
/// 舊資料冒充新鮮不是。
///
/// 存的是 age 而不是 `Instant`：render 才好被測試餵任意年紀（§4 的 stale
/// 顯示要驗得到），狀態機與 view 也不必各自算時間。
///
/// **兩軸的契約強度不同，型別與文案都據實分開**（審查 F3）：
/// - disk 只說得出「上次**完成**一輪掃描」。`Model::load` 不回 `Result`，
///   `read_dir` 失敗與單檔損壞都靜默跳過——它抓得到迴圈被拖住，抓不到部分讀
///   失敗，所以**不得**宣稱 success。
/// - tmux 說得出「上次**成功**」：查詢有明確的失敗訊號（整層降級 `None`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Freshness {
    /// 距上次**完成**一輪磁碟掃描（完成 ≠ 每一筆都讀成功）
    pub disk: Duration,
    /// 距上次**成功**的 tmux 查詢輪；`None`＝**至今沒有任何成功樣本**
    /// （啟動後第一輪還沒回來，或回來的每一輪都是降級的 unknown）。
    /// 用 `Option` 而不是「拿啟動時間充當 stamp」：後者會在前 10 秒顯示
    /// 「距啟動多久」並冒充新鮮，那是無中生有的背書（審查 F2）。
    pub tmux: Option<Duration>,
}

/// disk 軸的 stale 門檻。磁碟輪詢是 500ms，取 3s＝六個週期：一兩輪被慢磁碟或
/// 一次長按鍵處理拖過去不該報警，連續六輪沒完成就不是抖動而是迴圈真的卡住了。
pub const DISK_STALE: Duration = Duration::from_secs(3);
/// tmux 軸的 stale 門檻。liveness 節流是 2s，取 10s＝五個週期，且高於單次
/// bounded 查詢的逾時上限——低於它會把「這一輪剛好慢」誤報成 stale。
pub const TMUX_STALE: Duration = Duration::from_secs(10);
/// age 低於這個數就不顯示數字（低噪）：正常態每一幀的 age 都在跳動，逐秒
/// 顯示只會讓人習慣性忽略它，等真的 stale 了也不會注意到。門檻取各軸的
/// stale 門檻之半——會顯示數字時，代表它已經在往 stale 走了。
pub fn age_is_worth_showing(age: Duration, stale_at: Duration) -> bool {
    age * 2 >= stale_at
}

impl Freshness {
    /// tmux 軸是否已舊到該降級為 unknown（run loop 用；disk 軸沒有對應的降級
    /// 動作——它沒有「別的值可以退回去」，只有 footer 上的 stale 標記）。
    ///
    /// **沒有成功樣本（`None`）一律算 stale**：那時畫面上根本沒有可信的死活
    /// 可畫，unknown 才是誠實的說法。
    pub fn tmux_stale(&self) -> bool {
        self.tmux.is_none_or(|age| age >= TMUX_STALE)
    }
}

/// tmux liveness 快照（§4：節流每 2s，且每條查詢 bounded——逾時整層降級
/// `None`＝unknown，MUST NOT 凍結 UI）。
pub struct LiveIndex {
    /// pane id → 所有出現位置 `(session_name, window_id)`（linked window 下
    /// 同一 pane 可出現多次，cardinality 不可丟——§2 focus 語意要用）。
    pub panes: Option<HashMap<String, Vec<(String, String)>>>,
    /// 現存 window id → 其**全部**出現位置 `(session_name, window_name)`。
    ///
    /// 帶 name 是 P4.6 題 1：origin 列要顯示人看得懂的 `session:window-name`，
    /// 而不是 `@108` 這種只有 tmux 認得的 id。**沿用既有那一條 list-windows
    /// 查詢、只擴 format**（§4 bounded-read：不新增 round trip）。
    ///
    /// 為什麼是 `Vec` 而不是單筆：window 可同時 linked 到多個 session
    /// （`man tmux`「Windows may be linked to multiple sessions」），`-a` 列表
    /// 因此對同一個 `@id` 出現多次。存單筆＝依 tmux 列序任選最後一筆，那正是
    /// CLI-LIST-2「cardinality 不可丟」擋下的事（`spawn.rs::live_label` 同一
    /// 條紀律）。
    pub windows: Option<HashMap<String, Vec<(String, String)>>>,
    /// `list-panes` 的結果**實際到手的那一刻**（審查 F1）。
    ///
    /// 為什麼觀測時間要跟著快照走、而不是由 UI 在收信時取 `Instant::now()`：
    /// 一輪 `Msg::Live` 要跑完 list-panes → list-windows → 逐 pane 兩次 bounded
    /// blocker 查詢，單次逾時預設 5 秒，整輪可以遠超過 `TMUX_STALE`。用收信
    /// 時間算 age，等於把十幾秒前的 pane 快照重新標成「剛更新」——正是這一項
    /// 要修掉的缺陷。放在結構裡而不是另傳一個參數，是為了讓它**不可能被忘記
    /// 帶上**。
    ///
    /// 取 panes 這一筆是刻意的保守值：它是整輪最先發出的查詢，因此是本輪所有
    /// 子快照裡**最舊**的觀測時間。`None`＝這一輪沒有成功的 pane 快照。
    pub panes_at: Option<Instant>,
}

impl LiveIndex {
    pub fn query(tmux: &dyn TmuxClient) -> Self {
        let panes = tmux
            .exec(&[
                "list-panes",
                "-a",
                "-F",
                "#{pane_id}\t#{session_name}\t#{window_id}",
            ])
            .and_then(|o| o.ok_stdout())
            .map(|out| {
                let mut map: HashMap<String, Vec<(String, String)>> = HashMap::new();
                for line in out.lines() {
                    let mut it = line.splitn(3, '\t');
                    if let (Some(p), Some(s), Some(w)) = (it.next(), it.next(), it.next()) {
                        map.entry(p.to_string())
                            .or_default()
                            .push((s.to_string(), w.to_string()));
                    }
                }
                map
            });
        // pane 快照到手的那一刻就記下來——後面的 list-windows 與逐 pane blocker
        // 查詢每一條都可能各等一次 bounded 逾時
        let panes_at = panes.as_ref().map(|_| Instant::now());
        let windows = tmux
            .exec(&[
                "list-windows",
                "-a",
                "-F",
                "#{session_name}\t#{window_id}\t#{window_name}",
            ])
            .and_then(|o| o.ok_stdout())
            .map(|out| {
                let mut map: HashMap<String, Vec<(String, String)>> = HashMap::new();
                for line in out.lines() {
                    let mut it = line.splitn(3, '\t');
                    // window name 可以是空字串（tmux 允許），但欄位本身必須在
                    // ——少一欄代表這行不是我們要的形狀，寧可整行丟掉也不猜
                    if let (Some(s), Some(w), Some(n)) = (it.next(), it.next(), it.next()) {
                        map.entry(w.to_string())
                            .or_default()
                            .push((s.to_string(), n.to_string()));
                    }
                }
                // 同一位置重複列出（linked window 的 name 在各 session 相同）
                // 不該被算成兩個位置：排序去重之後剩下的才是真正的 cardinality
                for locs in map.values_mut() {
                    locs.sort();
                    locs.dedup();
                }
                map
            });
        LiveIndex {
            panes,
            windows,
            panes_at,
        }
    }

    /// 全 unknown 的空快照。UI 的起始值就是它——第一輪 liveness 由背景
    /// worker 回報，UI thread 一次 tmux 都不自己查（審查 F1）。
    pub fn unknown() -> Self {
        LiveIndex {
            panes: None,
            windows: None,
            panes_at: None,
        }
    }

    /// 這一輪算不算「成功拿到 tmux 快照」，算的話回傳**觀測時間**
    /// （審查 F1／F2）。
    ///
    /// 兩個必要子項都要成功：`panes` 撐死活軸、`windows` 撐 ORIGINS 欄的
    /// window 狀態。只有 panes 成功就刷新整個 tmux age，等於讓 list-panes
    /// 替 list-windows 背書——畫面上 origin 列全是 unknown，footer 卻寫著
    /// 「2s」。
    ///
    /// **空集合仍算成功**：`list-panes` 回了一份沒有任何 pane 的結果，那是
    /// 一個成立的觀測（機器上真的沒有 pane），與查詢失敗是兩回事。
    ///
    /// blocker 軸**刻意不列入成功判定**：單一 pane 已經死掉時 `blocker_of`
    /// 合法地回 `Unknown`，把它算成失敗會讓 registry 裡留著一個死 pane 就
    /// 永遠標不出新鮮——那是比誤報更糟的誤報。
    pub fn success_at(&self) -> Option<Instant> {
        match (&self.panes, &self.windows) {
            (Some(_), Some(_)) => self.panes_at,
            _ => None,
        }
    }
}

/// BLOCKER 軸（tui-design §4 的雙軸狀態：ACTIVITY × BLOCKER 正交疊加）。
///
/// v1 契約**只承諾兩件事**，兩者都有現成實作來源（§4／§7 終審收窄）：
/// - `Prompt`：沿用 `notify::screen_has_prompt` 的硬編碼 matcher
///   （permission／plan 兩類框，含 agy 與下緣備援錨）
/// - `Occluded`：**結構性查詢** `pane_in_mode`（AB-COPYMODE-1），不靠畫面比對
///   ——copy-mode 是人在看，不是 worker 閒著
///
/// `Unknown` MUST 與 `None` 分開（顯示紀律 §5）：查不到不等於「沒有 blocker」。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Blocker {
    /// 查得到、沒有可見 blocker
    None,
    /// 權限／計畫確認框（`screen_has_prompt`）
    Prompt,
    /// pane 在 copy-mode 等 tmux mode（結構性，不是畫面比對）
    Occluded,
    /// tmux 查不到（逾時／不可用／pane 已不在）
    Unknown,
}

/// 一輪 blocker 查詢的快照。與 `LiveIndex` 同一輪節流（2s），每條查詢
/// bounded（§4 bounded-read 硬條款）。
pub struct BlockerIndex {
    /// pane id → blocker。整層查不到時為 `None`（全 unknown）
    pub panes: Option<HashMap<String, Blocker>>,
}

impl BlockerIndex {
    /// 對指定 pane 逐一查詢。**只查傳進來的 pane**（呼叫端給的是 registry
    /// 快照裡的 pane）：dashboard 每 2s 一輪，對整台機器的 pane 掃描是白工。
    pub fn query(tmux: &dyn TmuxClient, panes: &[String]) -> Self {
        let mut map = HashMap::new();
        for p in panes {
            if p.is_empty() {
                continue;
            }
            map.insert(p.clone(), blocker_of(tmux, p));
        }
        BlockerIndex { panes: Some(map) }
    }

    pub fn unknown() -> Self {
        BlockerIndex { panes: None }
    }

    pub fn get(&self, pane: &str) -> Blocker {
        match &self.panes {
            None => Blocker::Unknown,
            Some(m) => m.get(pane).copied().unwrap_or(Blocker::Unknown),
        }
    }
}

/// screen-matcher 來源的 blocker 要**連續命中這麼多輪**才升旗。
///
/// 理由：`Prompt` 靠畫面字串比對，單幀就可能是助理輸出剛好寫出那些字
/// （2026-08-01 語料：一行 `rg` 指令回顯就湊齊三組特徵）。matcher 收窄後
/// 這類幀已大幅減少，但畫面比對本質上沒有上層可否決——去抖是第二道。
/// 延遲代價照實記：升旗是「**首次命中後再等一輪**」。以 2s 一輪計，框可能
/// 剛好在一輪剛掃完之後才出現，**自框出現算起最壞未滿 4s**（一輪等到首次
/// 命中、再一輪確認），**不是 2s**——低報一半會讓下一個人以為餘裕比實際多。
/// 假警報則最多閃一輪就消失。worker 停在權限框是**分鐘級**的等待，4s 對人的
/// 判斷沒有影響。
///
/// 結構性判定（`Occluded`／`pane_in_mode`）**不去抖**：它不是字串比對，
/// 沒有單幀誤判面，去抖只會白白延後。
const BLOCKER_DEBOUNCE_ROUNDS: u32 = 2;

/// 跨輪去抖狀態：pane → 連續命中輪數。**只存 TUI 記憶體**（同 §4 的 occluded
/// 前值紀律），重開從零起算。
#[derive(Default)]
pub struct BlockerDebounce {
    streak: HashMap<String, u32>,
}

impl BlockerDebounce {
    pub fn new() -> Self {
        Self::default()
    }

    /// 丟掉所有連勝（審查 F7）。
    ///
    /// 「連續兩輪」這個條件隱含**時間鄰近性**：以 2s 一輪計，升旗代表框在
    /// 最壞未滿 4s 的窗口裡連著出現兩次。tmux 軸停擺（stale 降級）期間畫面
    /// 走 unknown，但 streak 若原樣留著，停擺前 streak=1 的 pane 會在停擺
    /// 30 秒後的**第一則**回報就立刻升旗——那兩次命中之間隔了半分鐘，與去抖
    /// 想證明的事完全無關。缺口一出現就把連勝作廢，重新起算。
    pub fn reset(&mut self) {
        self.streak.clear();
    }

    /// 把一輪原始判定過一次去抖，回傳要拿去顯示的索引。
    ///
    /// - `Prompt`：連續第 `BLOCKER_DEBOUNCE_ROUNDS` 輪起才放行；在那之前回
    ///   `None`（「查得到、沒有可見 blocker」）。**不是** `Unknown`——這一輪
    ///   畫面確實讀到了，謊報成「沒訊號」會讓 §5 的三態語意失真。
    /// - 其餘（`None`／`Occluded`／`Unknown`）原樣通過，並把連勝歸零：
    ///   **降旗即時**，一輪沒命中就撤，不欠人一個「還要再等一輪」。
    /// - 不在本輪索引裡的 pane 會被丟掉，streak 不會無界成長。
    pub fn apply(&mut self, raw: BlockerIndex) -> BlockerIndex {
        let Some(panes) = raw.panes else {
            // 整層查不到：連勝全清（下一輪重新起算），維持 unknown
            self.streak.clear();
            return BlockerIndex { panes: None };
        };
        let mut next = HashMap::with_capacity(panes.len());
        let mut out = HashMap::with_capacity(panes.len());
        for (pane, blocker) in panes {
            let shown = if blocker == Blocker::Prompt {
                let n = self.streak.get(&pane).copied().unwrap_or(0) + 1;
                next.insert(pane.clone(), n);
                if n >= BLOCKER_DEBOUNCE_ROUNDS {
                    Blocker::Prompt
                } else {
                    Blocker::None
                }
            } else {
                blocker
            };
            out.insert(pane, shown);
        }
        self.streak = next;
        BlockerIndex { panes: Some(out) }
    }
}

/// 單一 pane 的 blocker 判定（純粹是兩次 bounded 查詢的組合，可用假件單測）。
///
/// 先問結構性的 copy-mode 再看畫面：copy-mode 下的 `capture-pane` 拿到的是
/// 人捲到的位置，拿它判 prompt 只會是誤判。查不到一律 `Unknown`——**MUST NOT
/// 當成 `None`**（§5：沒有訊號 ≠ 沒有 blocker）。
pub fn blocker_of(tmux: &dyn TmuxClient, pane: &str) -> Blocker {
    match tmux.pane_in_mode(pane) {
        Some(true) => return Blocker::Occluded,
        Some(false) => {}
        None => return Blocker::Unknown,
    }
    match tmux.capture_pane(pane) {
        Some(screen) if ab_core::notify::screen_has_prompt(&screen) => Blocker::Prompt,
        Some(_) => Blocker::None,
        None => Blocker::Unknown,
    }
}

/// 三態死活：查不到（tmux 逾時／不可用）≠ 查了但不在（同 `list --long` 的
/// 顯示紀律：誤標 dead 會讓人以為該回收）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Liveness {
    Live,
    Dead,
    Unknown,
}

pub fn pane_liveness(live: &LiveIndex, pane: &str) -> Liveness {
    if pane.is_empty() {
        return Liveness::Unknown;
    }
    match &live.panes {
        None => Liveness::Unknown,
        Some(m) => {
            if m.contains_key(pane) {
                Liveness::Live
            } else {
                Liveness::Dead
            }
        }
    }
}

/// origin 標籤所指 window 的狀態（P4.6 題 1／題 2）。
///
/// **這不是 agent 死活**：window 沒了不代表底下的 worker 沒了，反之亦然
/// （§11 根因判定）。故它只用文字進 DETAIL，不在 origin 列上畫 ●／✗
/// ——那個 glyph 正是「window 死活冒充 agent 死活」的來源。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WindowState {
    /// window 還在、且位置唯一：`(session_name, window_name)`，兩者都是
    /// **此刻查到的**
    Live(String, String),
    /// window 還在，但 linked 到多個 session 而**消歧義不出唯一的一筆**。
    /// 帶著全部位置——任選一筆就是冒名（CLI-LIST-2）。
    Ambiguous(Vec<(String, String)>),
    /// 查得到 window 集合、但這一個不在裡面。**不猜舊名**：只有 id 說得出口
    Gone,
    /// tmux 查不到（逾時／不可用）
    Unknown,
    /// 標籤根本不是 window 形（`-`＝manual、`?`＝registry 缺 owner 欄，
    /// 或 `s:@garbage` 這種**壞掉的** registry 值）
    NotAWindow,
}

/// origin 標籤 → `(session_prefix, window_id)`；不是 `<session>:@<digits>`
/// 形就回 `None`。
///
/// 形狀檢查與 `spawn.rs::is_valid_window_id` 同一套（`@` 後必須全是 ASCII
/// 數字、session 非空）：registry 是**人可手改的不可信輸入**，`s:@garbage`
/// 是資料損壞，不是「這個 window 沒了」——標成 Gone 會讓人以為東西曾經在。
fn split_origin(label: &str) -> Option<(&str, &str)> {
    let (sess, win) = label.rsplit_once(':')?;
    if sess.is_empty() {
        return None;
    }
    let digits = win.strip_prefix('@')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((sess, win))
}

/// `label` 所指 window 此刻的狀態。
///
/// linked window（同一 `@id` 出現在多個 session）以 origin 標籤記的 session
/// 名消歧義；**配不出唯一的一筆就 MUST NOT 任選**（`spawn.rs::live_label`
/// 同一條紀律）——顯示層據此降級成原始標籤，不冒用任何一個 session 的名字。
pub fn window_state(live: &LiveIndex, label: &str) -> WindowState {
    let Some((sess, win)) = split_origin(label) else {
        return WindowState::NotAWindow;
    };
    let Some(ws) = &live.windows else {
        return WindowState::Unknown;
    };
    let locs = match ws.get(win) {
        Some(v) if !v.is_empty() => v,
        _ => return WindowState::Gone,
    };
    if let [(s, n)] = locs.as_slice() {
        return WindowState::Live(s.clone(), n.clone());
    }
    // 判定錨在不可變的 `@id`，session 名只拿來消歧義（它可被 rename）
    let mut matched = locs.iter().filter(|(s, _)| s == sess);
    match (matched.next(), matched.next()) {
        (Some((s, n)), None) => WindowState::Live(s.clone(), n.clone()),
        _ => WindowState::Ambiguous(locs.clone()),
    }
}

/// DETAIL 欄的 window 行（長版：**完整 `@id` MUST 留著**，它才是介入時能拿去
/// 對 tmux 下命令的那個識別）。
///
/// - live → `session:window-name (@id, live)`
/// - ambiguous → `session:@id (linked: a:name, b:name)`——歧義**顯形**，
///   不替人挑一個（CLI-LIST-2 的 `a:1,b:1` 同一手法）
/// - gone → `session:@id (gone)`
/// - unknown → `session:@id (unknown)`
/// - 非 window 形 → `<label> (n/a)`（`-`／`?`／壞掉的 `@garbage` 沒有 window
///   可言，寫成 unknown 會被讀成「查不到」——那是另一回事）
pub fn window_detail(live: &LiveIndex, label: &str) -> String {
    match window_state(live, label) {
        WindowState::Live(sess, name) => {
            let win = split_origin(label).map(|(_, w)| w).unwrap_or("-");
            format!("{sess}:{name} ({win}, live)")
        }
        WindowState::Ambiguous(locs) => {
            let all: Vec<String> = locs.iter().map(|(s, n)| format!("{s}:{n}")).collect();
            format!("{label} (linked: {})", all.join(", "))
        }
        WindowState::Gone => format!("{label} (gone)"),
        WindowState::Unknown => format!("{label} (unknown)"),
        WindowState::NotAWindow => format!("{label} (n/a)"),
    }
}

/// WORKERS 欄的一列：worker 列或其下的 in-flight task 列（§2 selection
/// model：兩者皆可選取，task 列自帶 immutable task id）。值是 `Model`
/// 內的索引，不複製資料。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    Worker(usize),
    Task { worker: usize, task: usize },
}

/// WORKERS 欄一列的 **stable key**（P4.6 切片 B）：selection 存的是「選了誰」
/// 而不是「選了第幾列」。列序每 500ms 都可能變（別人在你上面插了一列、你選的
/// 那一列消失），純索引在那一刻會靜默指到另一個對象身上——而畫面上的
/// DETAIL、`x`／`e` 的目標全都跟著它走。
///
/// worker 用 `(name, spawn_tag)` 而不是只用名字：同名 respawn 是**新的一代**
/// （世代正是 evict CAS 的比對軸，§5），選取 MUST NOT 無聲接續到新代身上。
/// task 用 immutable task id。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RowKey {
    Worker { name: String, spawn_tag: String },
    Task(String),
}

/// 列 → 身分（stable key）。
pub fn row_key(model: &Model, row: Row) -> RowKey {
    match row {
        Row::Worker(wi) => RowKey::Worker {
            name: model.workers[wi].name.clone(),
            spawn_tag: model.workers[wi].spawn_tag.clone(),
        },
        Row::Task { task, .. } => RowKey::Task(model.tasks[task].id.clone()),
    }
}

/// 攤平 WORKERS 欄：依 lineage 分組的順序列出 worker 與其 in-flight task。
///
/// **組標頭不是一列 `Row`**（P4.7 切片 B 的設計取捨）：它是 render 端插進去
/// 的裝飾行。理由有兩條——(1) 組是分類不是對象，它身上沒有任何鍵可按，讓
/// 游標停在上面只是多一次 `j`（P4 的步數是 gate）；(2) selection 的語意因此
/// **逐字不變**：`rows` 仍然只有 worker 與 task 兩種，P4.6 的 stable key／
/// `fallback_row`／Enter matrix 一行都不必動。代價是 render 要把「第幾列」
/// 換算成「第幾行」（見 `group_line_offsets`／`worker_row_lines`），那是純函式。
///
/// worker 依組序、組內依 snapshot 序（檔名字典序），task 依 id 序。
pub fn worker_rows(model: &Model) -> Vec<Row> {
    let mut rows = Vec::new();
    for g in model.groups.iter() {
        for &wi in &g.members {
            rows.push(Row::Worker(wi));
            for (ti, t) in model.tasks.iter().enumerate() {
                if t.to == model.workers[wi].name {
                    rows.push(Row::Task {
                        worker: wi,
                        task: ti,
                    });
                }
            }
        }
    }
    rows
}

/// 每一個 `worker_rows` 列在畫面上的**行號**（組標頭佔行不佔列）。
///
/// 與 `worker_rows` 逐字同一個走訪順序——兩者一旦漂開，捲動就會把選取列推出
/// 畫面外，而那是靜默的（畫面照畫，只是看不到游標）。
pub fn worker_row_lines(model: &Model) -> Vec<usize> {
    let mut out = Vec::new();
    let mut line = 0usize;
    for g in model.groups.iter() {
        line += 1; // 組標頭：佔一行，不佔列
        for &wi in &g.members {
            out.push(line);
            line += 1;
            for t in model.tasks.iter() {
                if t.to == model.workers[wi].name {
                    out.push(line);
                    line += 1;
                }
            }
        }
    }
    out
}

/// `rows` 索引 → 「在它之前要插入哪一組的標頭」（render 端畫標頭用）。
pub fn group_line_offsets(model: &Model) -> HashMap<usize, usize> {
    let mut out = HashMap::new();
    let mut row = 0usize;
    for (gi, g) in model.groups.iter().enumerate() {
        // 空組不可達：`group_by_lineage` 只在有成員時才建組（lineage 組來自
        // `entry().or_default().push()`，standalone 段有 `is_empty()` 閘）。
        // 真的來了一個空組，這裡會讓兩組共用同一個 row key，標頭少畫一行
        debug_assert!(
            !g.members.is_empty(),
            "空組不可達（group_by_lineage 不產生）"
        );
        out.insert(row, gi);
        for &wi in &g.members {
            row += 1; // worker 列
            row += model
                .tasks
                .iter()
                .filter(|t| t.to == model.workers[wi].name)
                .count();
        }
    }
    out
}

/// 選取列在畫面上的行號（列號＋它前面所有組標頭）。
pub fn worker_line_of(model: &Model, row_idx: usize) -> usize {
    worker_row_lines(model)
        .get(row_idx)
        .copied()
        .unwrap_or(row_idx)
}

/// PgUp／PgDn 的目標列：以**一個 viewport 的 rendered lines** 為位移量。
///
/// 直接把可視行高當成「幾列」是錯的（切片 B1 修正輪 G2）：組標頭佔行不佔列，
/// 兩者一差，翻頁的落點就會**跳過**中間某些列——而且是永久跳過，那些列在
/// 任何一頁上都不出現。這裡改成先換算成行號、位移一個 viewport，再挑
/// 「行號跨過該位置的第一個／最後一個列」，翻頁於是與畫面逐行銜接。
pub fn worker_page_row(model: &Model, row_idx: usize, page_lines: usize, down: bool) -> usize {
    let lines = worker_row_lines(model);
    if lines.is_empty() {
        return 0;
    }
    let last = lines.len() - 1;
    let cur = lines.get(row_idx).copied().unwrap_or(0);
    let page = page_lines.max(1);
    if down {
        let target = cur + page;
        // 下一頁的頁首＝上一頁末列的下一列（行號 >= target 的第一個列）
        lines.iter().position(|&l| l >= target).unwrap_or(last)
    } else {
        let target = cur.saturating_sub(page);
        lines.iter().rposition(|&l| l <= target).unwrap_or(0)
    }
}

/// TASKS 欄的列：`model.recent` 的索引，順序沿用 `recent`（id 反序＝新的在上）。
///
/// **P4.7 切片 B 起不再過濾**：過濾軸原本是 ORIGINS 的 scope，而那個面板已
/// 退場。lineage-scoped 的 All／Unattached 是切片 C 的題目——在它落地之前，
/// 這一欄的語意就是舊的 `ALL`：全部，連收件人已不在 registry 的任務也留著
/// （worker 被回收之後，它的任務正是人最想找回來的東西）。
///
/// 附帶效果：`recent_truncated` 的 `+` 恆為誠實的——那是**全 pool** 的旗標，
/// 而這一欄現在也正是全 pool（P4.6 切片 C 的 F5 因此自然成立）。
pub fn task_rows(model: &Model) -> Vec<usize> {
    (0..model.recent.len()).collect()
}

/// 終態判定（`x` 的合法目標判斷用）。權威字沿用 `spec/state.md`。
pub fn is_terminal_status(st: &str) -> bool {
    matches!(st, "completed" | "failed" | "cancelled")
}

/// `Enter` focus 的執行計畫（§2）：目標在 current session→只 select；
/// 在別的 session→先 switch-client。linked window 多位置→優先 current
/// session 的 location，否則取第一個 live location；不彈詢問框。
/// 回傳 `None`＝pane 位置查不到（死了或 tmux unknown），呼叫端降級成訊息。
#[derive(PartialEq, Eq, Debug)]
pub struct FocusPlan {
    pub switch_to: Option<String>,
    pub window: String,
}

pub fn focus_plan(
    locs: Option<&Vec<(String, String)>>,
    current_session: Option<&str>,
) -> Option<FocusPlan> {
    let locs = locs?;
    let chosen = current_session
        .and_then(|cur| locs.iter().find(|(s, _)| s == cur))
        .or_else(|| locs.first())?;
    let switch_to = match current_session {
        Some(cur) if cur == chosen.0 => None,
        // tmux 外啟動（無 current session）也給出 switch 目標：有 client 就
        // 切得過去，沒 client 只是查詢失敗降級
        _ => Some(chosen.0.clone()),
    };
    Some(FocusPlan {
        switch_to,
        window: chosen.1.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(name: &str, spawned: bool, owner: &str) -> AgentSnapshot {
        AgentSnapshot {
            name: name.to_string(),
            pane: format!("%{}", name.len()),
            runtime: "codex".to_string(),
            owner: owner.to_string(),
            ready: "ready".to_string(),
            spawn_tag: format!("tag-{name}"),
            registered_at: "2026-07-31T00:00:00Z".to_string(),
            spawned,
            corrupt: false,
            // P4.7 切片 A：lineage 兩欄對這些 fixture 無關（None＝欄位缺席）
            lineage_root: None,
            parent_agent: None,
        }
    }

    /// 帶 lineage 兩欄的快照（P4.7 切片 B）。`tag` 是**裸的世代碼**，這裡
    /// 補上 canonical 前綴——測試要驗的是「歸組只比對全串」，用真的形狀才驗得到
    fn lin(name: &str, tag: &str, root: Option<&str>, parent: Option<&str>) -> AgentSnapshot {
        let key = |t: &str| format!("AGENT_BRIDGE_SPAWN_TAG=ab-spawn-{t}");
        AgentSnapshot {
            spawn_tag: key(tag),
            lineage_root: root.map(key),
            parent_agent: parent.map(key),
            ..snap(name, true, "it:@1")
        }
    }

    /// 三種 registry 形狀的捷徑：人工註冊（無 spawn_tag）／損壞。
    fn manual(name: &str) -> AgentSnapshot {
        AgentSnapshot {
            spawn_tag: String::new(),
            spawned: false,
            ..snap(name, false, "")
        }
    }

    /// 測試用 model：`groups` 一律由 `group_by_lineage` 推導，與 `load`
    /// 同一條路徑（fixture 自己寫一份分組＝驗自己抄的答案）。
    fn model_of(workers: Vec<AgentSnapshot>, tasks: Vec<InFlight>) -> Model {
        Model {
            groups: group_by_lineage(&workers),
            workers,
            tasks,
            recent: Vec::new(),
            recent_truncated: false,
        }
    }

    /// 裸世代碼 → canonical generation key（測試裡到處要用）
    fn canon(tag: &str) -> String {
        format!("AGENT_BRIDGE_SPAWN_TAG=ab-spawn-{tag}")
    }

    fn inflight(id: &str, to: &str) -> InFlight {
        InFlight {
            id: id.to_string(),
            from: "alice".to_string(),
            to: to.to_string(),
            status: "queued".to_string(),
        }
    }

    /// selection model（§2）：worker 列與其 in-flight task 列皆為可選取列，
    /// task 緊接在所屬 worker 之後。組標頭**不是列**（render 端裝飾），
    /// 但列序必須依 `model.groups` 分組排（P4.7 切片 B：ORIGINS 退場，
    /// lineage 成為唯一軸）。
    #[test]
    fn worker_rows_group_by_lineage_and_interleave_tasks() {
        let model = model_of(
            vec![
                lin(
                    "root",
                    "root-1-aaaaaaaaaaaa",
                    Some("root-1-aaaaaaaaaaaa"),
                    None,
                ),
                lin(
                    "w1",
                    "w1-2-bbbbbbbbbbbb",
                    Some("root-1-aaaaaaaaaaaa"),
                    Some("root-1-aaaaaaaaaaaa"),
                ),
                manual("manual"),
            ],
            vec![
                inflight("20260731T000001Z-aaaa", "w1"),
                inflight("20260731T000002Z-bbbb", "root"),
                inflight("20260731T000003Z-cccc", "w1"),
            ],
        );
        // 一個 lineage 組（root＋w1）＋一個 standalone 段（manual）
        assert_eq!(model.groups.len(), 2);
        assert_eq!(
            model.groups[0].key,
            GroupKey::Lineage(canon("root-1-aaaaaaaaaaaa"))
        );
        assert_eq!(model.groups[1].key, GroupKey::Standalone);
        assert_eq!(
            worker_rows(&model),
            vec![
                Row::Worker(0),
                Row::Task { worker: 0, task: 1 },
                Row::Worker(1),
                Row::Task { worker: 1, task: 0 },
                Row::Task { worker: 1, task: 2 },
                Row::Worker(2),
            ]
        );
    }

    /// **gate (a) 資料面**：lineage root→A→B→C，移除 A／B 的 registry 之後
    /// C 仍歸同一組——歸屬的證據是 C 自己身上的 generation key，不是「鏈上
    /// 每一節都還在」。
    #[test]
    fn a_lineage_survives_the_removal_of_its_middle_generations() {
        let root = "root-1-aaaaaaaaaaaa";
        let full = model_of(
            vec![
                lin("root", root, Some(root), None),
                lin("A", "a-2-bbbbbbbbbbbb", Some(root), Some(root)),
                lin(
                    "B",
                    "b-3-cccccccccccc",
                    Some(root),
                    Some("a-2-bbbbbbbbbbbb"),
                ),
                lin(
                    "C",
                    "c-4-dddddddddddd",
                    Some(root),
                    Some("b-3-cccccccccccc"),
                ),
            ],
            Vec::new(),
        );
        assert_eq!(full.groups.len(), 1, "四代同組");
        assert_eq!(full.groups[0].members, vec![0, 1, 2, 3]);

        // A／B 的 registry 被移除（例如 despawn）：C 照樣在同一組
        let pruned = model_of(
            vec![
                lin("root", root, Some(root), None),
                lin(
                    "C",
                    "c-4-dddddddddddd",
                    Some(root),
                    Some("b-3-cccccccccccc"),
                ),
            ],
            Vec::new(),
        );
        assert_eq!(pruned.groups.len(), 1, "中間兩代不在了，組還在");
        assert_eq!(
            pruned.groups[0].key,
            GroupKey::Lineage(canon(root)),
            "組別 key 不變（它是 generation key，不是誰還在場）"
        );
        assert_eq!(pruned.groups[0].members, vec![0, 1]);

        // **負向：不得由 task 推導歸組**。C 的 parent（B）不在場，而 task 的
        // `from` 恰好叫 "A"——若歸組偷看 task，這裡就會多出一組或改變成員
        let with_tasks = model_of(
            vec![
                lin("root", root, Some(root), None),
                lin(
                    "C",
                    "c-4-dddddddddddd",
                    Some(root),
                    Some("b-3-cccccccccccc"),
                ),
            ],
            vec![InFlight {
                id: "20260731T000009Z-zzzz".into(),
                from: "A".into(),
                to: "C".into(),
                status: "queued".into(),
            }],
        );
        assert_eq!(
            with_tasks.groups, pruned.groups,
            "task 的 from/to 不參與歸組"
        );
    }

    /// **gate (b) 資料面**：新舊混合的四型歸屬。
    #[test]
    fn legacy_manual_and_invalid_rows_follow_the_contract() {
        let root = "root-1-aaaaaaaaaaaa";
        // legacy＝兩欄缺席。第一個 legacy 的 spawn_tag **就是**某組的 root key
        // → 它是那一組的根；第二個誰也不是 → standalone
        let legacy_root = AgentSnapshot {
            lineage_root: None,
            parent_agent: None,
            ..lin("legacy-root", root, None, None)
        };
        let legacy_orphan = AgentSnapshot {
            lineage_root: None,
            parent_agent: None,
            ..lin("legacy-orphan", "orphan-9-999999999999", None, None)
        };
        // invalid：lineage_root 存在但不是 generation key（`Some("")` 是型別
        // 錯誤的 invalid 標記，型別面見 registry 的三態測試）
        let mut bad_root = lin("bad-root", "bad-5-eeeeeeeeeeee", Some(root), None);
        bad_root.lineage_root = Some(String::new());
        // invalid：自己是自己的 parent
        let self_parent = lin(
            "self-parent",
            "sp-6-ffffffffffff",
            Some(root),
            Some("sp-6-ffffffffffff"),
        );
        let model = model_of(
            vec![
                lin("child", "c-2-bbbbbbbbbbbb", Some(root), Some(root)),
                legacy_root,
                legacy_orphan,
                manual("manual"),
                bad_root,
                self_parent,
            ],
            Vec::new(),
        );
        let roots: std::collections::HashSet<String> =
            std::collections::HashSet::from([canon(root)]);
        let by_tag: HashMap<&str, &AgentSnapshot> = model
            .workers
            .iter()
            .map(|w| (w.spawn_tag.as_str(), w))
            .collect();
        let aff = |i: usize| worker_affiliation(&model.workers[i], &roots, &by_tag);
        assert_eq!(
            aff(0),
            Affiliation::Lineage(canon(root)),
            "新式列依 root 歸組"
        );
        assert_eq!(
            aff(1),
            Affiliation::Lineage(canon(root)),
            "legacy 且自身 tag ＝某組 root key → 它就是該組的根"
        );
        assert_eq!(
            aff(2),
            Affiliation::Legacy,
            "legacy 且誰也不是 → standalone"
        );
        assert_eq!(aff(3), Affiliation::Manual, "人工註冊沒有世代");
        assert_eq!(
            aff(4),
            Affiliation::Invalid,
            "lineage_root 不是 generation key"
        );
        assert_eq!(aff(5), Affiliation::Invalid, "自己是自己的 parent");

        // 版面：lineage 組一個（child＋legacy-root），其餘四列全進 standalone 段
        assert_eq!(model.groups.len(), 2);
        assert_eq!(model.groups[0].members, vec![0, 1]);
        assert_eq!(model.groups[1].key, GroupKey::Standalone);
        assert_eq!(model.groups[1].members, vec![2, 3, 4, 5]);
    }

    /// **形狀驗證（切片 B1 修正輪 G1）**：半殘的 registry 列 MUST NOT 被當成
    /// legacy／manual 放行，而且 **invalid 列宣告的 root 不得進 `roots`**。
    ///
    /// 四條各自對應一個反例。少了它們，一列自相矛盾的 registry 就能無中生有
    /// 一個組別，或把一個 legacy 列吸進不存在的 lineage。
    #[test]
    fn half_shaped_rows_are_invalid_and_never_contaminate_the_roots() {
        let root = "root-1-aaaaaaaaaaaa";

        // (1) spawned/tag 不一致：沒有 spawn_tag 卻宣稱自己是 spawn 出來的
        let mut ghost = manual("ghost");
        ghost.spawned = true;
        // (1b) 沒有 spawn_tag 卻帶 lineage 欄
        let mut rootless = manual("rootless");
        rootless.lineage_root = Some(canon(root));

        // (2) parent-only：說得出上一代，卻說不出自己屬於哪一條 lineage
        let parent_only = AgentSnapshot {
            lineage_root: None,
            ..lin("parent-only", "po-2-bbbbbbbbbbbb", None, Some(root))
        };

        // (3) foreign-root-without-parent：root 指向別人，卻不說自己是誰的子代
        //     ——等於憑空認領一個組別
        let foreign = lin("foreign", "fr-3-cccccccccccc", Some(root), None);

        let model = model_of(
            vec![ghost, rootless, parent_only, foreign, manual("real-manual")],
            Vec::new(),
        );
        let by_tag: HashMap<&str, &AgentSnapshot> = model
            .workers
            .iter()
            .filter(|w| !w.spawn_tag.is_empty())
            .map(|w| (w.spawn_tag.as_str(), w))
            .collect();
        let roots: std::collections::HashSet<String> = std::collections::HashSet::new();
        let aff = |i: usize| worker_affiliation(&model.workers[i], &roots, &by_tag);
        assert_eq!(
            aff(0),
            Affiliation::Invalid,
            "spawned:true 卻沒有 spawn_tag"
        );
        assert_eq!(
            aff(1),
            Affiliation::Invalid,
            "沒有 spawn_tag 卻帶 lineage 欄"
        );
        assert_eq!(aff(2), Affiliation::Invalid, "parent-only：半殘形狀");
        assert_eq!(
            aff(3),
            Affiliation::Invalid,
            "foreign-root-without-parent：憑空認領組別"
        );
        assert_eq!(aff(4), Affiliation::Manual, "三項都對得上才是 manual");
        // 五列全 standalone，一個 lineage 組都不得生出來
        assert_eq!(model.groups.len(), 1);
        assert_eq!(model.groups[0].key, GroupKey::Standalone);
        assert_eq!(model.groups[0].members, vec![0, 1, 2, 3, 4]);
    }

    /// (4) **invalid-root-contaminates-legacy**：唯一提到 `R` 的那一列自己是
    /// invalid（foreign-root-without-parent），`R` 於是 MUST NOT 進 `roots`
    /// ——否則 spawn_tag 剛好等於 `R` 的那個 legacy 列會被吸進一個沒有任何
    /// 合法成員的組。
    #[test]
    fn an_invalid_rows_root_never_creates_a_group_for_a_legacy_row() {
        let r = "r-1-aaaaaaaaaaaa";
        // legacy：兩欄缺席，spawn_tag 恰好就是 R
        let legacy = AgentSnapshot {
            lineage_root: None,
            parent_agent: None,
            ..lin("legacy", r, None, None)
        };
        // invalid：root 指向 R，卻沒有 parent（憑空認領）
        let bad = lin("bad", "bad-2-bbbbbbbbbbbb", Some(r), None);
        let model = model_of(vec![legacy, bad], Vec::new());

        assert_eq!(
            model.groups.len(),
            1,
            "MUST NOT 生出 Lineage(R) 組（實際：{:?}）",
            model.groups
        );
        assert_eq!(model.groups[0].key, GroupKey::Standalone);
        assert_eq!(model.groups[0].members, vec![0, 1], "兩列都落 standalone");

        // 對照組：換成一個**形狀合法**的子代（有 parent、root 一致），R 才
        // 成為組別 key，legacy 列也才被吸附進來
        let good = lin("good", "good-2-cccccccccccc", Some(r), Some(r));
        let ok = model_of(
            vec![
                AgentSnapshot {
                    lineage_root: None,
                    parent_agent: None,
                    ..lin("legacy", r, None, None)
                },
                good,
            ],
            Vec::new(),
        );
        assert_eq!(ok.groups.len(), 1);
        assert_eq!(ok.groups[0].key, GroupKey::Lineage(canon(r)));
        assert_eq!(ok.groups[0].members, vec![0, 1]);
    }

    /// B6（歸組會用到的那部分）：兩欄是 **worker 可寫面**，不得假設它們構成
    /// 一棵樹。cycle 與跨 lineage 不一致都不得把兩列併進同一組。
    #[test]
    fn a_cycle_or_cross_lineage_parent_never_forms_a_group() {
        let ra = "ra-1-aaaaaaaaaaaa";
        let rb = "rb-1-bbbbbbbbbbbb";
        // A→B→A 的 cycle：兩列互為 parent，且各自宣稱自己是根
        let model = model_of(
            vec![
                lin("A", "a-2-cccccccccccc", Some(ra), Some("b-3-dddddddddddd")),
                lin("B", "b-3-dddddddddddd", Some(ra), Some("a-2-cccccccccccc")),
            ],
            Vec::new(),
        );
        // 同一個 lineage_root，所以**歸組是合法的**——cycle 的傷害在 traversal
        // （breadcrumb 會繞不完），那是切片 B2 的防護矩陣。這裡釘住的是
        // 「歸組不因為 cycle 就崩潰或分裂」
        assert_eq!(model.groups.len(), 1);
        assert_eq!(model.groups[0].members, vec![0, 1]);

        // 跨 lineage 不一致：C 說自己屬於 ra，但它的 parent 屬於 rb。
        // P 是 rb 這一組的根（無 parent 的新式列 MUST root ＝自身 tag，
        // 見形狀驗證），所以它自己是合法的——不合法的只有 C
        let model = model_of(
            vec![
                lin("P", rb, Some(rb), None),
                lin("C", "c-3-ffffffffffff", Some(ra), Some(rb)),
            ],
            Vec::new(),
        );
        let roots: std::collections::HashSet<String> =
            std::collections::HashSet::from([canon(ra), canon(rb)]);
        let by_tag: HashMap<&str, &AgentSnapshot> = model
            .workers
            .iter()
            .map(|w| (w.spawn_tag.as_str(), w))
            .collect();
        assert_eq!(
            worker_affiliation(&model.workers[1], &roots, &by_tag),
            Affiliation::Invalid,
            "兩欄互相矛盾時不猜哪一邊對"
        );
        assert_eq!(model.groups[1].key, GroupKey::Standalone);
        assert_eq!(model.groups[1].members, vec![1], "invalid 列 standalone");
    }

    /// TASKS 欄（P4.7 切片 B）：**不再過濾**——scope 軸隨 ORIGINS 一起退場，
    /// lineage-scoped 的 All／Unattached 是切片 C 的題目。
    #[test]
    fn task_rows_keep_every_recent_task_including_orphans() {
        let mut model = model_of(vec![lin("w1", "w1-2-bbbbbbbbbbbb", None, None)], Vec::new());
        model.recent = vec![
            inflight("20260731T000003Z-cccc", "w1"),
            // 收件人已不在 registry：**MUST 留著**（worker 被回收之後，
            // 它的任務正是人最想找回來的東西）
            inflight("20260731T000002Z-bbbb", "gone"),
        ];
        assert_eq!(task_rows(&model), vec![0, 1]);
        assert!(is_terminal_status("completed") && is_terminal_status("cancelled"));
        assert!(!is_terminal_status("running"));
    }

    /// §2 focus 語意逐條：current session 優先、跨 session 才 switch、
    /// linked window 多位置不彈框、查不到＝None。
    #[test]
    fn focus_plan_follows_design_semantics() {
        let locs = vec![
            ("other".to_string(), "@7".to_string()),
            ("cur".to_string(), "@3".to_string()),
        ];
        // current session 的 location 優先，且不 switch
        assert_eq!(
            focus_plan(Some(&locs), Some("cur")),
            Some(FocusPlan {
                switch_to: None,
                window: "@3".to_string()
            })
        );
        // current session 沒有 → 取第一個 live location，先 switch
        assert_eq!(
            focus_plan(Some(&locs), Some("elsewhere")),
            Some(FocusPlan {
                switch_to: Some("other".to_string()),
                window: "@7".to_string()
            })
        );
        // 查不到位置（pane 死了／tmux unknown）→ None（降級，不凍結不猜）
        assert_eq!(focus_plan(None, Some("cur")), None);
        assert_eq!(focus_plan(Some(&Vec::new()), Some("cur")), None);
    }

    /// blocker 判定用的假 tmux：可分別調 `pane_in_mode` 與 `capture_pane`
    /// 的回應（含 `None`＝bounded 查詢逾時／不可用）。
    struct BlockerTmux {
        in_mode: Option<bool>,
        screen: Option<&'static str>,
    }

    impl TmuxClient for BlockerTmux {
        fn exec(&self, _args: &[&str]) -> Option<ab_core::tmux::TmuxOutput> {
            None
        }
        fn available(&self) -> bool {
            true
        }
        fn resolve_pane(&self, _t: &str) -> Option<String> {
            None
        }
        fn pane_exists(&self, _p: &str) -> bool {
            true
        }
        fn capture_pane(&self, _p: &str) -> Option<String> {
            self.screen.map(|s| s.to_string())
        }
        fn pane_in_mode(&self, _p: &str) -> Option<bool> {
            self.in_mode
        }
        fn send_keys(&self, _p: &str, _k: &str) -> bool {
            false
        }
    }

    /// BLOCKER 軸的 v1 契約（§4）：硬編碼 prompt matcher ＋結構性 occlusion，
    /// 且 **unknown MUST 與 none 分開**（§5：沒有訊號 ≠ 沒有 blocker）。
    #[test]
    fn blocker_axis_keeps_v1_contract_and_three_states() {
        // 真的長得像 claude 權限框（`screen_has_prompt` 的第一組錨）
        let prompt = "Do you want to make this edit?\n 1. Yes\n 2. No\nEsc to cancel";
        let cases: [(Option<bool>, Option<&'static str>, Blocker); 6] = [
            // copy-mode 是結構性判定，先於畫面：人在看，不是 worker 閒著
            (Some(true), Some(prompt), Blocker::Occluded),
            (Some(true), None, Blocker::Occluded),
            // 非 copy-mode → 看畫面
            (Some(false), Some(prompt), Blocker::Prompt),
            (Some(false), Some("just some output\n$ "), Blocker::None),
            // 畫面查不到（bounded 逾時）→ unknown，MUST NOT 當成 none
            (Some(false), None, Blocker::Unknown),
            // 連 mode 都查不到 → unknown
            (None, Some(prompt), Blocker::Unknown),
        ];
        for (in_mode, screen, want) in cases {
            let tmux = BlockerTmux { in_mode, screen };
            assert_eq!(
                blocker_of(&tmux, "%1"),
                want,
                "in_mode={in_mode:?} screen={screen:?}"
            );
        }
    }

    /// `BlockerIndex`：只查傳進來的 pane；沒查過的與整層失效都是 unknown。
    #[test]
    fn blocker_index_defaults_to_unknown_outside_the_queried_set() {
        let tmux = BlockerTmux {
            in_mode: Some(false),
            screen: Some("idle output"),
        };
        let idx = BlockerIndex::query(&tmux, &["%1".to_string(), String::new()]);
        assert_eq!(idx.get("%1"), Blocker::None);
        // 空 pane 不查（registry 缺 pane_id），也不會被當成 none
        assert_eq!(idx.get(""), Blocker::Unknown);
        assert_eq!(idx.get("%404"), Blocker::Unknown, "沒查過的 pane＝unknown");
        // 整層 unknown（UI 起始值／worker 還沒回報）
        assert_eq!(BlockerIndex::unknown().get("%1"), Blocker::Unknown);
    }

    /// 便利建構：`(session, @id, name)` 逐列 → window 索引（同 id 多列＝
    /// linked window）。
    fn win_index(rows: &[(&str, &str, &str)]) -> HashMap<String, Vec<(String, String)>> {
        let mut map: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for (s, w, n) in rows {
            map.entry(w.to_string())
                .or_default()
                .push((s.to_string(), n.to_string()));
        }
        for locs in map.values_mut() {
            locs.sort();
            locs.dedup();
        }
        map
    }

    /// 三態死活：查不到 ≠ 不在（顯示紀律，tui-design §5）。
    #[test]
    fn liveness_keeps_unknown_distinct_from_dead() {
        let unknown = LiveIndex::unknown();
        assert!(matches!(pane_liveness(&unknown, "%1"), Liveness::Unknown));
        assert_eq!(window_state(&unknown, "it:@1"), WindowState::Unknown);

        let mut panes = HashMap::new();
        panes.insert("%1".to_string(), vec![("it".to_string(), "@1".to_string())]);
        let live = LiveIndex {
            panes: Some(panes),
            windows: Some(win_index(&[("it", "@1", "main")])),
            ..LiveIndex::unknown()
        };
        assert!(matches!(pane_liveness(&live, "%1"), Liveness::Live));
        assert!(matches!(pane_liveness(&live, "%9"), Liveness::Dead));
        assert_eq!(
            window_state(&live, "it:@1"),
            WindowState::Live("it".into(), "main".into())
        );
        assert_eq!(window_state(&live, "it:@9"), WindowState::Gone);
        // manual（`-`）與 `?` 不是 window 形：**與 unknown 分開**（前者是
        // 「沒有 window 可言」，後者是「查不到」）
        assert_eq!(window_state(&live, "-"), WindowState::NotAWindow);
        assert_eq!(window_state(&live, "?"), WindowState::NotAWindow);
    }

    /// P4.6 題 1：origin 標籤誠實化。三態各自的字面都要驗到——
    /// **gone MUST NOT 猜舊名**（只說得出 id），unknown MUST NOT 說成 gone。
    #[test]
    fn origin_labels_tell_the_truth_in_all_three_window_states() {
        let live = LiveIndex {
            panes: None,
            windows: Some(win_index(&[("scratch", "@108", "main")])),
            ..LiveIndex::unknown()
        };
        // live：人看得懂的 session:window-name；DETAIL 仍留完整 @id
        assert_eq!(
            window_detail(&live, "scratch:@108"),
            "scratch:main (@108, live)"
        );
        // gone：沿用原標籤（session＋@id），一個字的舊名都不猜
        assert_eq!(window_detail(&live, "old:@9"), "old:@9 (gone)");
        // unknown（tmux 查不到）MUST NOT 被寫成 gone
        let unknown = LiveIndex::unknown();
        assert_eq!(window_detail(&unknown, "old:@9"), "old:@9 (unknown)");
        // 非 window 形：列上原樣，DETAIL 標 n/a（不是「查不到」）
        for label in ["-", "?"] {
            assert_eq!(window_detail(&live, label), format!("{label} (n/a)"));
        }
    }

    /// **linked window（CLI-LIST-2「cardinality 不可丟」）**：同一個 `@id`
    /// 出現在多個 session 時，MUST 以 origin 標籤記的 session 消歧義；配不出
    /// 唯一的一筆就 MUST NOT 任選——顯示降級成原始標籤，並在 DETAIL 把全部
    /// 位置攤開。
    ///
    /// 存單筆 `(session, name)` 的實作在這裡會依 tmux 列序任選最後一筆，
    /// 三個 case 至少中一個。
    #[test]
    fn linked_windows_are_disambiguated_by_session_never_guessed() {
        let live = LiveIndex {
            panes: None,
            windows: Some(win_index(&[
                ("alpha", "@7", "work"),
                ("beta", "@7", "work-linked"),
                ("solo", "@8", "only"),
            ])),
            ..LiveIndex::unknown()
        };

        // (a) 標籤的 session 與其中恰一筆相符 → 取那一筆（不是列序最後一筆）
        assert_eq!(
            window_state(&live, "alpha:@7"),
            WindowState::Live("alpha".into(), "work".into())
        );
        assert_eq!(
            window_state(&live, "beta:@7"),
            WindowState::Live("beta".into(), "work-linked".into())
        );

        // (b) 標籤的 session 誰都配不上（session 被 rename／registry 記的是
        //     舊名）→ **不得任選**
        let amb = window_state(&live, "gamma:@7");
        assert_eq!(
            amb,
            WindowState::Ambiguous(vec![
                ("alpha".into(), "work".into()),
                ("beta".into(), "work-linked".into()),
            ])
        );
        let detail = window_detail(&live, "gamma:@7");
        assert!(
            detail.contains("alpha:work") && detail.contains("beta:work-linked"),
            "DETAIL MUST 讓歧義顯形（全部位置攤開）：{detail}"
        );
        for banned in ["(live)", "(gone)", "(unknown)"] {
            assert!(
                !detail.contains(banned),
                "歧義 MUST NOT 被說成 {banned}：{detail}"
            );
        }

        // (c) 同一 session 出現兩次（同 id、同 session、不同名——理論上的
        //     畸形輸入）：仍是配不出唯一，照樣不猜
        let dup = LiveIndex {
            panes: None,
            windows: Some(win_index(&[("alpha", "@9", "a"), ("alpha", "@9", "b")])),
            ..LiveIndex::unknown()
        };
        assert!(matches!(
            window_state(&dup, "alpha:@9"),
            WindowState::Ambiguous(_)
        ));

        // 單一位置照舊直接 Live（消歧義只在多位置時才發動）
        assert_eq!(
            window_state(&live, "whatever:@8"),
            WindowState::Live("solo".into(), "only".into())
        );

        // 同一位置被列兩次（linked window 在兩個 session 名字相同）不算兩個
        // 位置：去重之後仍是唯一
        let same = LiveIndex {
            panes: None,
            windows: Some(win_index(&[
                ("alpha", "@7", "work"),
                ("alpha", "@7", "work"),
            ])),
            ..LiveIndex::unknown()
        };
        assert_eq!(
            window_state(&same, "alpha:@7"),
            WindowState::Live("alpha".into(), "work".into())
        );
    }

    /// 畸形 origin 標籤是**資料損壞**，不是「window 沒了」：`@` 後必須至少
    /// 一位 ASCII 數字、session 非空（形狀檢查與 `spawn.rs::is_valid_window_id`
    /// 同一套）。標成 Gone 會讓人以為東西曾經在、現在沒了。
    #[test]
    fn malformed_origin_labels_are_not_a_window_never_gone() {
        let live = LiveIndex {
            panes: None,
            windows: Some(win_index(&[("s", "@1", "main")])),
            ..LiveIndex::unknown()
        };
        for bad in ["s:@", "s:@garbage", ":@1", "s:@1x", "@1", "-", "?"] {
            assert_eq!(
                window_state(&live, bad),
                WindowState::NotAWindow,
                "畸形／非 window 形標籤「{bad}」"
            );
            assert_eq!(window_detail(&live, bad), format!("{bad} (n/a)"));
        }
        // 對照組：形狀合法但不存在 → 這才是 Gone
        assert_eq!(window_state(&live, "s:@2"), WindowState::Gone);
    }

    /// window 名稱由**同一條** list-windows 查詢帶回（§4 bounded-read：不新增
    /// round trip）。假件記下所有呼叫，斷言恰好兩條查詢且 format 帶了三欄。
    #[test]
    fn live_index_reads_window_names_without_an_extra_round_trip() {
        struct RecordingTmux(std::sync::Mutex<Vec<Vec<String>>>);
        impl TmuxClient for RecordingTmux {
            fn exec(&self, args: &[&str]) -> Option<ab_core::tmux::TmuxOutput> {
                self.0
                    .lock()
                    .unwrap()
                    .push(args.iter().map(|s| s.to_string()).collect());
                let stdout = if args.contains(&"list-windows") {
                    "scratch\t@108\tmain\nit\t@2\tside\n".to_string()
                } else {
                    "%1\tscratch\t@108\n".to_string()
                };
                Some(ab_core::tmux::TmuxOutput {
                    status_ok: true,
                    stdout,
                    stderr: String::new(),
                })
            }
            fn available(&self) -> bool {
                true
            }
            fn resolve_pane(&self, _t: &str) -> Option<String> {
                None
            }
            fn pane_exists(&self, _p: &str) -> bool {
                true
            }
            fn capture_pane(&self, _p: &str) -> Option<String> {
                None
            }
            fn pane_in_mode(&self, _p: &str) -> Option<bool> {
                None
            }
            fn send_keys(&self, _p: &str, _k: &str) -> bool {
                false
            }
        }
        let tmux = RecordingTmux(std::sync::Mutex::new(Vec::new()));
        let live = LiveIndex::query(&tmux);
        let calls = tmux.0.lock().unwrap();
        assert_eq!(calls.len(), 2, "MUST 仍只有兩條 tmux 查詢：{calls:?}");
        assert!(
            calls.iter().any(|c| c
                .iter()
                .any(|a| a == "#{session_name}\t#{window_id}\t#{window_name}")),
            "list-windows 的 format MUST 一次帶回三欄：{calls:?}"
        );
        assert_eq!(
            window_detail(&live, "scratch:@108"),
            "scratch:main (@108, live)"
        );
        assert_eq!(window_detail(&live, "it:@2"), "it:side (@2, live)");
        assert_eq!(pane_liveness(&live, "%1"), Liveness::Live);
    }

    fn idx(pane: &str, b: Blocker) -> BlockerIndex {
        let mut m = HashMap::new();
        m.insert(pane.to_string(), b);
        BlockerIndex { panes: Some(m) }
    }

    /// 去抖升旗：screen-matcher 來源的 `Prompt` MUST 連續 K 輪才升起。
    ///
    /// 沒有這條，把 `BLOCKER_DEBOUNCE_ROUNDS` 改成 1 一樣全綠——而那正是
    /// 「單幀指令回顯 → 常駐假 ⛔blocked」的回歸。
    #[test]
    fn prompt_blocker_needs_consecutive_rounds_to_raise() {
        let mut d = BlockerDebounce::new();
        // 第一輪命中：還不升旗（顯示成「沒有可見 blocker」，不是 unknown）
        assert_eq!(d.apply(idx("%1", Blocker::Prompt)).get("%1"), Blocker::None);
        // 第二輪仍命中：升旗
        assert_eq!(
            d.apply(idx("%1", Blocker::Prompt)).get("%1"),
            Blocker::Prompt
        );
        // 持續命中維持升旗
        assert_eq!(
            d.apply(idx("%1", Blocker::Prompt)).get("%1"),
            Blocker::Prompt
        );
    }

    /// 單輪閃現不升旗，且連勝會被中斷的那一輪歸零（不得累加跨越間斷）。
    #[test]
    fn a_single_frame_never_raises_and_streaks_do_not_carry_over() {
        let mut d = BlockerDebounce::new();
        assert_eq!(d.apply(idx("%1", Blocker::Prompt)).get("%1"), Blocker::None);
        assert_eq!(d.apply(idx("%1", Blocker::None)).get("%1"), Blocker::None);
        // 歸零後再命中一次仍不足以升旗
        assert_eq!(d.apply(idx("%1", Blocker::Prompt)).get("%1"), Blocker::None);
    }

    /// 降旗**即時**：升旗後一輪未命中就撤，不欠人一個「再等一輪」。
    #[test]
    fn lowering_the_flag_is_immediate() {
        let mut d = BlockerDebounce::new();
        d.apply(idx("%1", Blocker::Prompt));
        assert_eq!(
            d.apply(idx("%1", Blocker::Prompt)).get("%1"),
            Blocker::Prompt
        );
        assert_eq!(d.apply(idx("%1", Blocker::None)).get("%1"), Blocker::None);
    }

    /// 結構性判定不受去抖影響：`Occluded`（`pane_in_mode`）第一輪就要顯示，
    /// `Unknown` 也 MUST NOT 被去抖改寫成 `None`（§5：沒有訊號 ≠ 沒有 blocker）。
    #[test]
    fn structural_verdicts_bypass_the_debounce() {
        let mut d = BlockerDebounce::new();
        assert_eq!(
            d.apply(idx("%1", Blocker::Occluded)).get("%1"),
            Blocker::Occluded
        );
        assert_eq!(
            d.apply(idx("%2", Blocker::Unknown)).get("%2"),
            Blocker::Unknown
        );
        // 整層查不到時維持 unknown
        assert_eq!(d.apply(BlockerIndex::unknown()).get("%1"), Blocker::Unknown);
    }

    /// 每個 pane 各自計數：一個 pane 的連勝不得幫另一個 pane 升旗。
    #[test]
    fn debounce_streaks_are_per_pane() {
        let mut d = BlockerDebounce::new();
        let mut m = HashMap::new();
        m.insert("%1".to_string(), Blocker::Prompt);
        m.insert("%2".to_string(), Blocker::None);
        d.apply(BlockerIndex { panes: Some(m) });

        let mut m2 = HashMap::new();
        m2.insert("%1".to_string(), Blocker::Prompt);
        m2.insert("%2".to_string(), Blocker::Prompt);
        let out = d.apply(BlockerIndex { panes: Some(m2) });
        assert_eq!(out.get("%1"), Blocker::Prompt); // 連續兩輪
        assert_eq!(out.get("%2"), Blocker::None); // 才第一輪
    }
}
