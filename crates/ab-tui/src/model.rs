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

    // `worker_idx(name)`（純名字 → 索引）在切片 C 修正輪 F3 之後**刪掉**：
    // 它是「task 屬於誰」這個判定的舊入口，而名字不是身分（同名 respawn 是
    // 新的一代、同名多列是壞資料）。唯一的入口是 `worker_of_task`
    // ——留著一個沒人用的名字比對，下一個人就會又拿它去接一條捷徑
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

/// `spawn_tag` → 該列的索引，**重複的 tag 一律不進來**（B2 修正輪 H2）。
///
/// 為什麼重複要當成「查不到」而不是「取一列」：`spawn_tag` 是這套系統唯一的
/// 身分軸，兩列同 tag 時**沒有任何證據**說得出哪一列才是那個世代——HashMap
/// 靜默留下最後寫入的那一列，等於用目錄序決定身分（切片 A 對 ambiguous
/// parent 已經裁過同一件事：不依目錄序任選）。
pub struct TagIndex<'a> {
    /// 唯一可證的列
    uniq: HashMap<&'a str, &'a AgentSnapshot>,
    /// 出現過兩次以上的 tag（**不可證**，查詢一律回 `None`）
    dup: std::collections::HashSet<&'a str>,
}

impl<'a> TagIndex<'a> {
    pub fn build(workers: &'a [AgentSnapshot]) -> Self {
        let mut uniq: HashMap<&'a str, &'a AgentSnapshot> = HashMap::new();
        let mut dup: std::collections::HashSet<&'a str> = Default::default();
        for w in workers
            .iter()
            .filter(|w| !w.corrupt && !w.spawn_tag.is_empty())
        {
            let k = w.spawn_tag.as_str();
            if dup.contains(k) {
                continue;
            }
            if uniq.insert(k, w).is_some() {
                // 第二次看到：把已收的那一列也撤掉——「先來的算數」同樣是
                // 目錄序決定身分
                uniq.remove(k);
                dup.insert(k);
            }
        }
        TagIndex { uniq, dup }
    }

    /// 唯一可證的那一列。缺席與重複回**同一個答案**（`None`）：兩者都是
    /// 「這份 registry 說不出這個世代是誰」，畫面上也就都是墓碑。
    fn get(&self, key: &str) -> Option<&'a AgentSnapshot> {
        self.uniq.get(key).copied()
    }

    fn is_dup(&self, key: &str) -> bool {
        self.dup.contains(key)
    }
}

/// 逐列判形狀。**registry 兩欄是 worker 可寫面**，所以每一種缺漏／矛盾都要
/// 有明確歸屬，不能靠「大概是舊版寫的」放行。
fn row_shape(w: &AgentSnapshot, idx: &TagIndex) -> Shape {
    if w.corrupt {
        return Shape::Invalid;
    }
    // H2：自己的 tag 就是重複的 → 這一列的身分無法證明，不參與任何組
    if !w.spawn_tag.is_empty() && idx.is_dup(&w.spawn_tag) {
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
    // H1.2：有 spawn_tag 卻不是 spawn 出來的。spawn 寫入必帶 `spawned:true`
    // （切片 A 的 parent 比對也只認 `spawned == true` 的列），這一列於是在說
    // 一件寫入路徑產不出來的事
    if !w.spawned {
        return Shape::Invalid;
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
                    // H1.1：自稱是根、卻又寫著 parent。兩欄互相矛盾（根的
                    // 定義就是「沒有 parent」），不猜哪一邊對
                    if root == w.spawn_tag {
                        return Shape::Invalid;
                    }
                    // parent 在場時要能接得上（重複 tag ＝不可證，見 `TagIndex`）
                    if let Some(pw) = idx.get(p) {
                        match valid_root(pw) {
                            // 分屬不同 lineage：兩欄互相矛盾，不猜哪一邊對
                            Some(proot) if proot != root => return Shape::Invalid,
                            // H1.3：parent 說不出自己的 lineage（legacy／值不合
                            // 文法）。切片 A 的 fallback 語意是「退 parent 自身
                            // spawn_tag」，所以子代的 root **只能**是 parent 的
                            // tag；指向別的 R 等於憑空生出一個組
                            None if root != p => return Shape::Invalid,
                            _ => {}
                        }
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
/// `idx` 是「spawn_tag → 該列」的索引（`TagIndex`：重複的 tag 查不到），
/// `roots` 是本輪**已通過形狀驗證的** lineage 列所宣告的組別 key。兩者都由
/// 呼叫端在同一份快照上建好——registry 兩欄是 worker 可寫面，**不得假設它們
/// 構成一棵樹**。
pub fn worker_affiliation(
    w: &AgentSnapshot,
    roots: &std::collections::HashSet<String>,
    idx: &TagIndex,
) -> Affiliation {
    match row_shape(w, idx) {
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
/// 三趟：`TagIndex`（不依賴任何判定）→ 逐列形狀＋收 `roots`（**只收形狀合法
/// 的 lineage 列**）→ legacy 吸附。第二、三趟不能合併：legacy 要不要歸組取決
/// 於全部子代看完之後的 `roots`。
pub fn group_by_lineage(workers: &[AgentSnapshot]) -> Vec<Group> {
    let idx = TagIndex::build(workers);
    // 第一趟：所有**說得出口的**組別 key。invalid 列的 root 不進來——否則
    // 一列自相矛盾的 registry 就能讓一個 legacy 列被吸進不存在的組
    let roots: std::collections::HashSet<String> = workers
        .iter()
        .filter_map(|w| match row_shape(w, &idx) {
            Shape::Lineage(r) => Some(r),
            _ => None,
        })
        .collect();

    // 第二趟：逐列判歸屬。lineage 組依 key 字典序（畫面每輪都要穩定），
    // standalone 段恆在最後
    let mut lineage: std::collections::BTreeMap<String, Vec<usize>> = Default::default();
    let mut standalone: Vec<usize> = Vec::new();
    for (i, w) in workers.iter().enumerate() {
        match worker_affiliation(w, &roots, &idx) {
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

/// 「說不出世代」的畫面字面（**組標頭與 DETAIL 共用同一份**，切片 C）。
///
/// 同一件事在兩個位置寫成兩種樣子（`-` 與 `(standalone)`），人得自己猜它們
/// 是不是同一回事——那是畫面在製造問題，不是在回答問題。
pub const STANDALONE_LABEL: &str = "(standalone)";

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
        return STANDALONE_LABEL.to_string();
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

/// DETAIL breadcrumb 的一節（P4.7 切片 B2）。
///
/// `Gap` 是**斷層**不是節點：它說的是「這中間還有幾代，但這份 registry 說不
/// 出是誰」。畫成省略號而不是猜一個名字——猜出來的祖先跟真的長得一模一樣。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Crumb {
    Node {
        /// 畫面字面。在場＝agent 名；缺席＝`<name>† (<hex4>)` 墓碑
        label: String,
        /// 這一節的 registry 列不在場（純 display 標記，不參與任何判定）
        tombstone: bool,
    },
    /// 中間有斷不掉的未知世代（`root → … → parent`）
    Gap,
}

/// breadcrumb traversal 的 hop 上限：**當輪 registry 列數＋1**。
///
/// 為什麼是列數＋1：鏈上每一個「走得過去」的節點都必須是一列在場且沒走過的
/// registry 列（最多就是列數），最後**還可以再收一個缺席的祖先**當墓碑——那一
/// 節不在場，走不下去，但它是 self 兩欄裡寫著的證據，不該被上限吃掉。
///
/// 誠實話：有了 `visited` 之後這條界線在正常路徑上到不了。留著是因為兩道防護
/// 擋的是不同的東西——`visited` 擋「回到走過的節點」，這條擋「節點數本身失
/// 控」，而 traversal 的輸入是 worker 可寫面。拔掉 `visited` 時它就是唯一讓
/// cycle 停下來的東西（見 `a_cycle_cannot_walk_forever`）。
fn hop_limit(model: &Model) -> usize {
    model.workers.len() + 1
}

/// **DETAIL breadcrumb**：`root → … → parent† → self`（P4.7 切片 B2）。
///
/// 純函式（吃快照回節點序列），render 只投影。硬界線逐條：
/// - **僅由 `lineage_root`／`parent_agent` 兩欄重建**。不看 task 的 `from`／
///   `to`（那是訊息往來不是出身）、不看檔名、不由名字查表——每一跳都是
///   generation key 全串比對。名字只在**產生 label 的最後一步**出現。
/// - 節點在場（有一列 `spawn_tag` 等於該 key）→ 顯示它的 agent 名；缺席 →
///   `tag_display_parts` 的墓碑形式（唯一合法的 tag 剖析點，且僅 display）。
/// - 走不上去就**省略號**：第一個缺席的祖先之後，這份 registry 對「它的
///   parent 是誰」沒有任何證據，於是 `root` 與該節之間補一個 `Gap`。
/// - 形狀不合法的列（Legacy／Manual／Invalid）**沒有 breadcrumb**（`None`）
///   ——它們身上沒有說得出口的世代，畫一條線出來就是無中生有。
///
/// B6 防護：`visited` 擋 cycle（A→B→A）、`hop_limit` 擋鏈長失控、`row_shape`
/// 擋自指／自稱根卻有 parent／跨 lineage 矛盾（一律 `None`）、`TagIndex` 擋
/// 重複 tag（不可證＝墓碑）。
pub fn breadcrumb(model: &Model, w: &AgentSnapshot) -> Option<Vec<Crumb>> {
    let idx = TagIndex::build(&model.workers);
    let Shape::Lineage(root) = row_shape(w, &idx) else {
        return None;
    };

    // 自 self 沿 parent 往上走，收 generation key。走得動的條件有三：這一節
    // 不是 root、它說得出 parent、那個 parent 唯一可證——任何一項不成立就停
    let limit = hop_limit(model);
    let mut chain: Vec<&str> = vec![w.spawn_tag.as_str()];
    let mut visited: std::collections::HashSet<&str> =
        std::collections::HashSet::from([w.spawn_tag.as_str()]);
    let mut cur = w;
    // root 早停：形狀驗證之後（H1.1：自稱根就不得有 parent）根本身沒有 parent，
    // 走到根自然會停在下面那個 `else break`——這條於是是**冗餘的**。留著是因為
    // 迴圈的終止條件不該依賴另一個函式的不變量
    while cur.spawn_tag != root && chain.len() < limit {
        // 這裡不再驗 parent 的文法：`row_shape` 已對 self 驗過，往上每一節都是
        // 唯一可證的 registry 列，取它的 parent 前一樣要通過同一道形狀驗證
        let Some(p) = cur.parent_agent.as_deref() else {
            break;
        };
        if !visited.insert(p) {
            break;
        }
        chain.push(p);
        let Some(pw) = idx.get(p) else {
            // parent 缺席**或 tag 重複**：它的 parent 是誰，這份 registry 一個
            // 字都說不出來（重複＝有兩列自稱是它，證不出哪一列才是）
            break;
        };
        // 在場的祖先也要說得通才續走（半殘的中繼列不得把 self 的線拉長）
        if !matches!(row_shape(pw, &idx), Shape::Lineage(ref r) if *r == root) {
            break;
        }
        cur = pw;
    }

    let reached_root = chain.last() == Some(&root.as_str());
    let mut out: Vec<Crumb> = Vec::new();
    if !reached_root {
        out.push(crumb_of(&root, &idx));
        out.push(Crumb::Gap);
    }
    out.extend(chain.iter().rev().map(|k| crumb_of(k, &idx)));
    Some(out)
}

/// generation key → 一節的畫面字面（唯一可證用名、缺席／重複立墓碑）。
fn crumb_of(key: &str, idx: &TagIndex) -> Crumb {
    match idx.get(key) {
        Some(w) => Crumb::Node {
            label: w.name.clone(),
            tombstone: false,
        },
        None => Crumb::Node {
            label: match tag_display_parts(key) {
                Some((name, code)) => format!("{name}\u{2020} ({code})"),
                // 連剖析都失敗：只說得出「這一節存在」，不編故事（同 group_label）
                None => "?\u{2020}".to_string(),
            },
            tombstone: true,
        },
    }
}

/// breadcrumb 的畫面字串：節點以 ` → ` 相接，斷層畫成 `…`。
///
/// 放 model 層是因為它是**字面契約**（gate (a) 的 `root → … → B† → C`）：
/// 驗它的測試不該被迫先起一個 terminal。
pub fn breadcrumb_line(crumbs: &[Crumb]) -> String {
    crumbs
        .iter()
        .map(|c| match c {
            Crumb::Node { label, .. } => label.as_str(),
            Crumb::Gap => "\u{2026}",
        })
        .collect::<Vec<_>>()
        .join(" \u{2192} ")
}

/// 在**寬度上限內**的 breadcrumb 字面（B2 修正輪 H3）。
///
/// 底條模式（DETAIL 走整寬、只有 `DETAIL_STRIP_H` 那麼高）下 breadcrumb 一旦
/// 換行，就會把底下的等價 CLI 原文推出畫面——而那一行是薄殼原則的憑證。
/// 收縮順序（全部 display-only，不動節點序列本身）：
/// 1. 整條放得下 → 原樣
/// 2. 放不下 → **只留 root 與 self**，中間一律 `…`（兩端是這條線最說得出口
///    的兩節：從哪裡來、現在是誰）
/// 3. 連兩端都放不下 → 硬截到寬度並以 `…` 收尾（寧可截也不換行；換行等於
///    畫面上少一條命令）
pub fn breadcrumb_line_fit(crumbs: &[Crumb], width: usize) -> String {
    let full = breadcrumb_line(crumbs);
    if full.chars().count() <= width {
        return full;
    }
    let ends: Vec<&Crumb> = match (crumbs.first(), crumbs.last()) {
        (Some(a), Some(b)) if crumbs.len() > 1 => vec![a, b],
        _ => return truncate_with_ellipsis(&full, width),
    };
    let collapsed = format!(
        "{} \u{2192} \u{2026} \u{2192} {}",
        breadcrumb_line(&[ends[0].clone()]),
        breadcrumb_line(&[ends[1].clone()])
    );
    if collapsed.chars().count() <= width {
        collapsed
    } else {
        truncate_with_ellipsis(&collapsed, width)
    }
}

/// 截到指定寬度，截掉的部分以 `…` 表示（尾行預覽的 overlay 也用它——
/// 兩處各寫一份截字邏輯就會有兩種「截到一半」的字）。
pub fn truncate_with_ellipsis(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if s.chars().count() <= width {
        return s.to_string();
    }
    let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
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
    /// 對指定 pane 逐一查詢，連命中的框內容一起帶出來（P5.4 blocker snippet）。
    ///
    /// **只查傳進來的 pane**（呼叫端給的是 registry 快照裡的 pane）：
    /// dashboard 每 2s 一輪，對整台機器的 pane 掃描是白工。
    ///
    /// **不新增任何 tmux round trip**：snippet 取自這一輪 `capture-pane` 已經
    /// 拿在手上的那一屏（§4 bounded-read）。
    pub fn query_with_snippets(
        tmux: &dyn TmuxClient,
        panes: &[String],
    ) -> (Self, HashMap<String, Vec<String>>) {
        let mut map = HashMap::new();
        let mut snips = HashMap::new();
        for p in panes {
            if p.is_empty() {
                continue;
            }
            let (b, snip) = blocker_probe(tmux, p);
            if let Some(lines) = snip {
                snips.insert(p.clone(), lines);
            }
            map.insert(p.clone(), b);
        }
        (BlockerIndex { panes: Some(map) }, snips)
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
/// 判定＋命中框的內容（P5.4）。內容只在 `Prompt` 那一支存在——其餘三態下
/// 「框內容」不是缺席，是根本沒有框。
pub fn blocker_probe(tmux: &dyn TmuxClient, pane: &str) -> (Blocker, Option<Vec<String>>) {
    match tmux.pane_in_mode(pane) {
        Some(true) => return (Blocker::Occluded, None),
        Some(false) => {}
        None => return (Blocker::Unknown, None),
    }
    match tmux.capture_pane(pane) {
        // snippet 與判定同源（`ab_core::notify` 內同一個 matcher）：一邊說
        // blocked、另一邊拿不到框，是最難查的那種畫面
        Some(screen) => match ab_core::notify::prompt_snippet(&screen) {
            Some(lines) => (Blocker::Prompt, Some(lines)),
            None => (Blocker::None, None),
        },
        None => (Blocker::Unknown, None),
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

/// `/` 的畫面過濾（P4.7 切片 C）。**純 UI 狀態**：不進 registry、不發任何
/// 查詢，只決定哪幾列畫得出來。
///
/// **literal substring、case-insensitive，沒有 regex／glob**：查詢 `a.c` 不得
/// 命中 `abc`。理由與 §4 matcher 收窄同一條——這是給人用的即時篩選，不是
/// 給人寫小程式的地方；引入 regex 就得回答「不合法的 pattern 怎麼辦」「災難性
/// 回溯怎麼辦」，而那兩題在一個每 500ms 重畫的面板上沒有好答案。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Filter {
    /// 查詢字串（空＝停用）。**原樣保存**，比對時才降大小寫
    pub query: String,
}

impl Filter {
    pub fn is_active(&self) -> bool {
        !self.query.is_empty()
    }

    /// literal substring（case-insensitive）。停用時一律命中。
    pub fn matches(&self, hay: &str) -> bool {
        if !self.is_active() {
            return true;
        }
        hay.to_lowercase().contains(&self.query.to_lowercase())
    }
}

/// TASKS 欄的 scope（P4.7 切片 C）。`All` 是現行語意（全 pool）。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum Scope {
    #[default]
    All,
    /// **沒有任何 registry 列**可證明掛得上去的 task（見 `attached`）
    Unattached,
}

impl Scope {
    pub fn toggled(self) -> Self {
        match self {
            Scope::All => Scope::Unattached,
            Scope::Unattached => Scope::All,
        }
    }

    /// footer／面板標題的字面（chrome 全英文）
    pub fn label(self) -> &'static str {
        match self {
            Scope::All => "all",
            Scope::Unattached => "unattached",
        }
    }
}

/// 這個 task 掛得上這一列 worker 嗎（P4.7 切片 C 的**單一事實源**）。
///
/// 判準：`task.to == w.name` **且** task 的 `created_at` **嚴格晚於**該 agent 的
/// `registered_at`。
///
/// **為什麼是嚴格 `>`**（修正輪 R2／F1）：磁碟上的時戳只到整秒（`now_iso`），
/// 所以「同一秒」同時涵蓋兩個真實順序，資料本身分不出來——
/// (a) worker 先註冊、同秒被派任務（該掛）；
/// (b) 舊 task 先建立、同秒有人 respawn 了同名 worker（**不得掛**）。
/// §9 契約逐字禁止的是後者（「同名 respawn 不自動附掛歷史 task」、「僅由當前
/// 同世代證據**可唯一連結**者入組」），也就是 false positive。而 false negative
/// 是**可見且可恢復**的：那筆 task 就躺在 Unattached scope 裡，人一眼看得到；
/// false positive 則是靜默的錯誤歸屬。兩害相權取可見的那個。
///
/// 代價照實記：`register` 之後**同一秒**立刻派出的任務會暫時落在 Unattached。
/// 秒級精度是磁碟契約給的上限，不為了這個重開 schema（已裁定）。
///
/// **為什麼是 metadata 的 `created_at` 而不是 task-id 前綴**（修正輪 R1）：
/// 語意上 `created_at` 就是「這個任務何時建立」，id 前綴只是它的衍生副本，
/// 而兩者**不保證一致**——分組 41 的 fixture 就有一份 id 寫 `00:41:41`、
/// `created_at` 寫 `04:41:42` 的任務。同一份 read model 對「這個 task 幾點
/// 生的」不該有兩套答案；`gc` 的年齡判定早就選過同一邊（`task.rs` 的
/// 「用 metadata 的 created_at 而不是目錄 mtime……created_at 是這個任務自己
/// 的事實」）。
///
/// 為什麼判準只能是時戳：task metadata 是與 bash 逐字對齊的 CLI 契約
/// （`version/task_id/from/to/created_at/updated_at/working_directory/status`），
/// 不得為了畫面加欄；registry 也不回填。時戳因此是現有資料裡**唯一**的同世代
/// 證據。
///
/// **任一側解析不出＝不可證＝不掛**（fail-closed）。解析走
/// `ab_core::time::parse_iso_to_epoch`——它驗 separator 位置**並做曆日
/// round-trip**（`2026-99-99T…`／`2026-02-31T…` 一律 `None`）。自己寫一份
/// 「刪掉 `-` 與 `:` 再數位數」的骨架檢查，等於讓 fail-closed 這個保證變成假的
/// （修正輪 R2／F2）。
/// **實作正本在 `ab_core::page::task_belongs_to`**（跨廠複核 should-fix 3）：
/// page 層要用同一條規則，而依賴方向是 `ab-tui → ab-core`，所以規則下沉、
/// 這裡只留一個轉呼叫。上面那一整段推理仍是這條規則的論證，別因為函式縮成
/// 一行就把它搬走。
pub fn attached(task: &InFlight, w: &AgentSnapshot) -> bool {
    ab_core::page::task_belongs_to(task, w)
}

/// 這一筆 task 說得出唯一的 worker 嗎（修正輪 R2／F3 的**單一事實源**）。
///
/// 回 `Some(index)` **只在恰好一列** `attached()` 成立時。0 筆＝無主
/// （Unattached scope 看得到的那些）、>1 筆＝registry 自相矛盾（同名多列），
/// 兩者都回 `None`。
///
/// 為什麼不能用 `worker_idx(&task.to)`（純名字比對，切片 C 之前的寫法）：
/// 那會讓 Unattached 的歷史 task 一被選中就「認領」當代的同名 worker——DETAIL
/// 顯示當代 pane／blocker、`i` 按得動、`c` 的 payload 帶當代 pane，而畫面上
/// 同一筆 task 在 WORKERS 欄明明沒有掛在它底下。同名多列時 `position()` 還會
/// 依檔序靜默選第一個，與「不從壞資料裡挑贏家」的裁定直接矛盾。
pub fn worker_of_task(model: &Model, task: &InFlight) -> Option<usize> {
    let mut hit = None;
    for (wi, w) in model.workers.iter().enumerate() {
        if attached(task, w) {
            if hit.is_some() {
                return None; // >1：不任選
            }
            hit = Some(wi);
        }
    }
    hit
}

/// 掛在這一列 worker 底下的 in-flight task（`model.tasks` 的索引）。
///
/// **三處走訪的唯一來源**（`worker_rows`／`worker_row_lines`／
/// `group_line_offsets`）：切片 B1 的三份手抄條件在同名跨 lineage 時會雙掛，
/// 而漂移的症狀是**靜默的**（選取列被推出畫面外，畫面照畫）。
pub fn tasks_of(model: &Model, wi: usize) -> impl Iterator<Item = usize> + '_ {
    let w = &model.workers[wi];
    model
        .tasks
        .iter()
        .enumerate()
        .filter(move |(_, t)| attached(t, w))
        .map(|(ti, _)| ti)
}

// ===== P5.3 資料層：兩行列的第二行素材 =====

/// elapsed 的顯示格式（P5.3）：`<60s`＝`Xs`、`<1h`＝`XmYs`、`<24h`＝`XhYm`、
/// 其餘 `Xd`。解析失敗／now 早於 created（時鐘倒退）＝`-`（fail-closed，
/// 不顯示負數）。`now` 由呼叫端注入——render 測試要決定性。
pub fn fmt_elapsed(created_iso: &str, now_epoch: i64) -> String {
    let Some(created) = ab_core::time::parse_iso_to_epoch(created_iso) else {
        return "-".to_string();
    };
    if now_epoch < created {
        return "-".to_string();
    }
    fmt_secs(now_epoch - created)
}

/// 秒數 → 顯示字串（`fmt_elapsed` 的內核；idle 時長／事件 ago 共用）。
pub fn fmt_secs(s: i64) -> String {
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else if s < 86_400 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d", s / 86_400)
    }
}

/// events.log 行（`<iso> <event>[ <detail>]`）的事件字。壞行＝`None`。
pub fn event_word(line: &str) -> Option<&str> {
    line.split_whitespace().nth(1)
}

/// events.log 行的「多久以前」。時戳解析不出／在未來＝`None`（fail-closed）。
pub fn event_ago(line: &str, now_epoch: i64) -> Option<String> {
    let ts = line.split_whitespace().next()?;
    let at = ab_core::time::parse_iso_to_epoch(ts)?;
    if now_epoch < at {
        return None;
    }
    Some(fmt_secs(now_epoch - at))
}

/// worker 兩行列第二行的素材來源（P5.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// 有 in-flight：`model.tasks` 索引。同 worker 多筆取 **id 最大＝最新**
    /// （id 前綴是 created_at 的衍生，字典序即時間序；兩者不一致的病態
    /// fixture 下也只是挑錯「最新」，不影響狀態機）。
    Current(usize),
    /// 閒置。`since`＝idle 基準 epoch（語義對齊 spawn.rs `IdleRow`：
    /// `max(最近 attached task 的 created_at, spawned_at)`；兩者皆解析不出＝
    /// `None`，顯示層退化）。`last`＝最近 attached 的 `model.recent` 索引
    /// （`recent` 有 `RECENT_LIMIT` 視窗：任務掉出視窗時退回 spawn 時刻，
    /// 這是已知近似，見 tui-design.md P5.3 記錄）。
    Idle {
        since: Option<i64>,
        last: Option<usize>,
    },
}

/// 這一列 worker 現在在幹嘛。歸屬一律走 `attached`（單一事實源），
/// **不用名字比對**（同名 respawn 是新的一代）。
pub fn worker_activity(model: &Model, wi: usize) -> Activity {
    if let Some(ti) = tasks_of(model, wi).max_by(|a, b| model.tasks[*a].id.cmp(&model.tasks[*b].id))
    {
        return Activity::Current(ti);
    }
    let w = &model.workers[wi];
    // recent 是 id 反序（新的在前）：第一筆 attached 即最近一輪
    let last = model.recent.iter().position(|t| attached(t, w));
    let last_at = last.and_then(|ri| ab_core::time::parse_iso_to_epoch(&model.recent[ri].created_at));
    let spawned_at = ab_core::time::parse_iso_to_epoch(&w.spawned_at);
    let since = match (last_at, spawned_at) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    Activity::Idle { since, last }
}

/// 顯示層淨化：控制字元一律換空白（首行是使用者內容，`\t`／ESC 進 ratatui
/// cell 會毀版面）。只在 ingest 做一次，render 不再處理。
pub fn scrub_for_display(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// run loop 持有的任務摘要快取（P5.3-1）。
///
/// - **首行**：request.md 只在 send 時寫一次 → 快取永不失效、同一 id 絕不
///   重讀（讀不到＝fail-closed 記空字串，同樣不重試——每 500ms 對缺檔任務
///   重複 open 是無界成本）。
/// - **尾事件**：events.log 為 append-only，以**檔長變更**判斷重讀
///   （每輪只付一次 `stat`，不付整檔讀）。
/// - **記憶體有界**：不在 `tasks ∪ recent` 的 id 一律剔除。
#[derive(Default)]
pub struct Summaries {
    first_lines: HashMap<String, String>,
    last_events: HashMap<String, (u64, String)>,
}

impl Summaries {
    /// 每輪磁碟載入後呼叫（UI thread，與 `Model::load` 同節奏）。
    pub fn sync(&mut self, paths: &Paths, model: &Model) {
        let want: std::collections::HashSet<&str> = model
            .tasks
            .iter()
            .chain(model.recent.iter())
            .map(|t| t.id.as_str())
            .collect();
        self.first_lines.retain(|k, _| want.contains(k.as_str()));
        self.last_events.retain(|k, _| want.contains(k.as_str()));
        for id in want {
            if !self.first_lines.contains_key(id) {
                let line = task::request_first_line(paths, id)
                    .map(|b| scrub_for_display(&String::from_utf8_lossy(&b)))
                    .unwrap_or_default();
                self.first_lines.insert(id.to_string(), line);
            }
            let len = std::fs::metadata(task::task_dir(paths, id).join("events.log"))
                .map(|m| m.len())
                .unwrap_or(0);
            let stale = self.last_events.get(id).map(|(l, _)| *l != len).unwrap_or(true);
            if stale {
                let line = task::last_event_line(paths, id)
                    .map(|l| scrub_for_display(&l))
                    .unwrap_or_default();
                self.last_events.insert(id.to_string(), (len, line));
            }
        }
    }

    /// 測試注入（不經磁碟）：render 測試要驗第二行內容，不該為此鋪一座
    /// tempdir。
    #[cfg(test)]
    pub fn seed(first: &[(&str, &str)], events: &[(&str, &str)]) -> Self {
        Summaries {
            first_lines: first
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            last_events: events
                .iter()
                .map(|(k, v)| (k.to_string(), (0, v.to_string())))
                .collect(),
        }
    }

    /// request.md 首行（顯示層已淨化；缺檔＝空字串）。
    pub fn first_line(&self, id: &str) -> &str {
        self.first_lines.get(id).map(|s| s.as_str()).unwrap_or("")
    }

    /// events.log 尾行（原格式 `<iso> <event>[ <detail>]`；缺＝空字串）。
    pub fn last_event(&self, id: &str) -> &str {
        self.last_events
            .get(id)
            .map(|(_, s)| s.as_str())
            .unwrap_or("")
    }
}

/// blocker 框的畫面內容快取（P5.4，裁定 7 吸收變體 D 的價值）。
///
/// **零新增 tmux 流量**：內容來自 blocker probe 那一輪**已經抓在手上**的
/// `capture-pane` 輸出，隨同一則 `Msg::Live` 帶回來，不另發查詢。
///
/// **只活在 TUI 記憶體、不落盤**（同 §4 occluded 前值的紀律）：它是一屏的
/// 觀測，不是任務資料；寫進 task-plane 會讓一份易失的畫面快照變成證據。
///
/// **降旗即清**：顯示層的 blocker 不再是 `Prompt` 的 pane，其 snippet 立刻
/// 移除——留著就是拿一份舊畫面替「現在被擋住」背書。
#[derive(Default)]
pub struct Snippets {
    panes: HashMap<String, Vec<String>>,
}

impl Snippets {
    /// 一輪 probe 的結果落地。`shown` 是**去抖之後**要顯示的那份索引：
    /// 去抖期間（首次命中、尚未升旗）畫面說的是「沒有可見 blocker」，那時
    /// 顯示框內容等於搶在判定之前先下結論。
    pub fn apply(&mut self, fresh: HashMap<String, Vec<String>>, shown: &BlockerIndex) {
        self.panes.extend(fresh);
        self.panes.retain(|p, _| shown.get(p) == Blocker::Prompt);
    }

    pub fn get(&self, pane: &str) -> Option<&[String]> {
        self.panes.get(pane).map(|v| v.as_slice())
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.panes.len()
    }
}

/// 一列 worker 的**注意力等級**（P5.4 triage）。
///
/// 排序用 `Ord`：`Blocked > Dead > Failed > None`（裁定 2）。**這是注意力軸，
/// 不是可刪度軸**（§5）——`None` 在最下面只代表「現在沒有需要人介入的訊號」，
/// 不代表可以回收；idle 之間也不互相排序。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum Severity {
    #[default]
    None,
    /// 最近一輪任務收在 failed
    Failed,
    /// pane 查了但不在（**不含** unknown：沒有訊號不是異常）
    Dead,
    /// 停在權限／計畫確認框，正在等人
    Blocked,
}

/// 單列的 severity。三個判準各自都是**既有事實**，不新造狀態：blocker 軸、
/// liveness 軸、最近一輪任務的終態。
pub fn worker_severity(
    model: &Model,
    wi: usize,
    live: &LiveIndex,
    blockers: &BlockerIndex,
) -> Severity {
    let w = &model.workers[wi];
    if blockers.get(&w.pane) == Blocker::Prompt {
        return Severity::Blocked;
    }
    // `Unknown` MUST NOT 浮頂：tmux 停擺時全體升等於整份排序作廢（§5 三態）
    if pane_liveness(live, &w.pane) == Liveness::Dead {
        return Severity::Dead;
    }
    if let Activity::Idle { last: Some(ri), .. } = worker_activity(model, wi)
        && model.recent[ri].status == "failed"
    {
        return Severity::Failed;
    }
    Severity::None
}

/// 一組的 severity＝組內最大值（空組回 `None`）。
pub fn group_severity(
    model: &Model,
    g: &Group,
    live: &LiveIndex,
    blockers: &BlockerIndex,
) -> Severity {
    g.members
        .iter()
        .map(|&wi| worker_severity(model, wi, live, blockers))
        .max()
        .unwrap_or_default()
}

/// **組間浮頂、組內不排**（裁定 2）。
///
/// 為什麼只動組序：組內序是 snapshot 序（檔名字典序），既是 fixture 斷言的
/// 地基、也是人記得住的位置；把它按狀態重排，等於每 2s 就重畫一次名單順序。
/// 組間用 **stable sort**，同 severity 的組維持原本的字典序／standalone 段
/// 相對位置。
///
/// 呼叫端 MUST 在**資料事件邊緣**呼叫（load 後／`Msg::Live` 後／stale 降級
/// 邊緣），並緊接 `App::relocate`——每幀重排會讓 selection 追著排序跑。
///
/// **排序前先回到 canonical 序**（`group_by_lineage`，跨廠複核 2026-08-05
/// finding 5）：對「已經排過的組」再做一次 stable sort，同分時保留的是上一輪
/// 的浮頂序——severity 消失之後組序就再也回不到原本的字典序，畫面於是留著
/// 一個沒有事實支撐的排名。從 canonical 序起排讓這個函式成為
/// (workers, live, blockers) 的**純函式**：與呼叫次數、與上一輪排成什麼樣
/// 都無關。
pub fn apply_triage(model: &mut Model, live: &LiveIndex, blockers: &BlockerIndex) {
    let mut sev: Vec<(Severity, Group)> = group_by_lineage(&model.workers)
        .into_iter()
        .map(|g| (group_severity(model, &g, live, blockers), g))
        .collect();
    // `sort_by_key` 是 stable：同分維持 canonical 序
    sev.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
    model.groups = sev.into_iter().map(|(_, g)| g).collect();
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
///
/// **`filter` 是同一趟走訪的一部分**（P4.7 切片 C）：過濾後的列序才是畫面上的
/// 列序，selection／捲動／stable key 全部吃它。
pub fn worker_rows(model: &Model, filter: &Filter) -> Vec<Row> {
    let mut rows = Vec::new();
    walk_workers(model, filter, |ev| {
        if let WalkEvent::Row(r) = ev {
            rows.push(r);
        }
    });
    rows
}

/// 每一個 `worker_rows` 列在畫面上的**行號**（組標頭佔行不佔列）。
///
/// 與 `worker_rows` 逐字同一個走訪順序——三處各抄一份條件是切片 B1 留下的
/// 債（verifier 留言：同名跨 lineage 會雙掛），而漂移的症狀是**靜默的**
/// （捲動把選取列推出畫面外，畫面照畫）。現在三者共用 `walk_workers`。
pub fn worker_row_lines(model: &Model, filter: &Filter) -> Vec<usize> {
    let mut out = Vec::new();
    let mut line = 0usize;
    walk_workers(model, filter, |ev| match ev {
        WalkEvent::GroupHead(_) => line += 1,
        WalkEvent::Row(r) => {
            // 記的是該列的**頂行**；捲動端要保證整列可見時自行加
            // `row_height - 1` 取底行
            out.push(line);
            line += row_height(r);
        }
    });
    out
}

/// 行高的**單一事實源**（P5.3 兩行列）：worker 列佔 2 行（第二行＝活動摘要），
/// 內嵌 task 子列維持 1 行——子列自己就是任務，摘要不重複。行號會計
/// （`worker_row_lines`）與畫線（`render_workers`）都吃這一份；固定值是
/// 行號會計單純性的前提（「極窄退化單行」因此不做，見 plan 裁定 4）。
pub fn row_height(row: Row) -> usize {
    match row {
        Row::Worker(_) => 2,
        Row::Task { .. } => 1,
    }
}

/// TASKS 欄的行高（P5.3 兩行列）——**該欄的單一事實源**。
///
/// 與 `row_height` **是兩件事**，故不共用：後者答的是 WORKERS 走訪裡某一列
/// 佔幾行（worker 2、內嵌 task 子列 1），這裡答的是 TASKS 那份平坦列表每列
/// 佔幾行（恆 2）。共用一個函式會把「WORKERS 底下的 task 子列」與「TASKS 欄
/// 的 task 列」當成同一種東西——它們的行高本來就不同。
///
/// 三個消費點 MUST 全部走這一份：`render_tasks` 推幾行、捲動餵哪個行號、
/// 捲軌總量幾行，加上 `app::page_move` 的一頁幾列。任一處寫死數字，症狀就是
/// 靜默跳列或 thumb 指錯位置（跨廠複核 2026-08-05 finding 3 實證）。
pub const TASK_ROW_H: usize = 2;

/// WORKERS 欄的總行數（組標頭＋各列行高）——捲軌總量的單一來源。
pub fn worker_lines_total(model: &Model, filter: &Filter) -> usize {
    let mut line = 0usize;
    walk_workers(model, filter, |ev| match ev {
        WalkEvent::GroupHead(_) => line += 1,
        WalkEvent::Row(r) => line += row_height(r),
    });
    line
}

/// pane 的人類可辨識位置 `<session>:<window-name>`（P5.3 裁定 8）。
///
/// 資料全部來自既有 `LiveIndex`（零新增 tmux 查詢）。多重 linked window 時
/// **不靜默任選**（CLI-LIST-2 cardinality 紀律）：取排序後第一個位置並綴
/// `+N` 標出其餘數量——「current session 優先」需要一條新查詢才知道自己在哪，
/// 不值得為顯示欄付（Enter focus 的 action 層照舊做完整判斷）。
/// window name 查不到＝退回 window id；pane 不在快照（dead／unknown）＝`None`，
/// 呼叫端退回裸 pane id。
pub fn pane_location_label(live: &LiveIndex, pane: &str) -> Option<String> {
    let locs = live.panes.as_ref()?.get(pane)?;
    if locs.is_empty() {
        return None;
    }
    let mut sorted: Vec<&(String, String)> = locs.iter().collect();
    sorted.sort();
    sorted.dedup();
    let (sess, wid) = sorted[0];
    let name = live
        .windows
        .as_ref()
        .and_then(|w| w.get(wid))
        .and_then(|ls| ls.iter().find(|(s, _)| s == sess))
        .map(|(_, n)| n.as_str())
        .filter(|n| !n.is_empty());
    let label = format!("{sess}:{}", name.unwrap_or(wid.as_str()));
    Some(if sorted.len() > 1 {
        format!("{label}+{}", sorted.len() - 1)
    } else {
        label
    })
}

/// `rows` 索引 → 「在它之前要插入哪一組的標頭」（render 端畫標頭用）。
///
/// 過濾後**只有還有存活列的組**才有標頭：一個空組的標頭等於畫面上宣稱有
/// 一組東西，而底下一列都沒有。
pub fn group_line_offsets(model: &Model, filter: &Filter) -> HashMap<usize, usize> {
    let mut out = HashMap::new();
    let mut row = 0usize;
    walk_workers(model, filter, |ev| match ev {
        WalkEvent::GroupHead(gi) => {
            out.insert(row, gi);
        }
        WalkEvent::Row(_) => row += 1,
    });
    out
}

/// 每一組**畫得出來的** worker 數（組標頭的括號數字）。
///
/// 過濾中要算可見的那些，不是 `members.len()`：標頭寫 `(6)` 而底下只有兩列，
/// 是畫面在說一件與自己相矛盾的事。
pub fn group_visible_members(model: &Model, filter: &Filter) -> HashMap<usize, usize> {
    let mut out: HashMap<usize, usize> = HashMap::new();
    let mut cur: Option<usize> = None;
    walk_workers(model, filter, |ev| match ev {
        WalkEvent::GroupHead(gi) => cur = Some(gi),
        WalkEvent::Row(Row::Worker(_)) => {
            if let Some(gi) = cur {
                *out.entry(gi).or_default() += 1;
            }
        }
        WalkEvent::Row(_) => {}
    });
    out
}

/// 一組裡**畫得出來的**成員各異常各幾個（P5.4 組標頭徽章）。
///
/// 三個欄位互斥：一列只算它自己的 severity（`worker_severity` 已定序），
/// 不會既算 blocked 又算 dead——重複計數會讓標頭的數字大於組員數。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct SeverityCounts {
    pub blocked: usize,
    pub dead: usize,
    pub failed: usize,
}

/// 每組的異常計數。與 `group_visible_members` 走**同一趟** filter 走訪：
/// 括號裡的成員數與徽章數若來自不同集合，篩選中的標頭會自相矛盾。
pub fn group_visible_severities(
    model: &Model,
    filter: &Filter,
    live: &LiveIndex,
    blockers: &BlockerIndex,
) -> HashMap<usize, SeverityCounts> {
    let mut out: HashMap<usize, SeverityCounts> = HashMap::new();
    let mut cur: Option<usize> = None;
    walk_workers(model, filter, |ev| match ev {
        WalkEvent::GroupHead(gi) => cur = Some(gi),
        WalkEvent::Row(Row::Worker(wi)) => {
            if let Some(gi) = cur {
                let e = out.entry(gi).or_default();
                match worker_severity(model, wi, live, blockers) {
                    Severity::Blocked => e.blocked += 1,
                    Severity::Dead => e.dead += 1,
                    Severity::Failed => e.failed += 1,
                    Severity::None => {}
                }
            }
        }
        WalkEvent::Row(_) => {}
    });
    out
}

/// 走訪過程中吐出的東西：組標頭（佔行不佔列）與可選取列。
enum WalkEvent {
    GroupHead(usize),
    Row(Row),
}

/// **WORKERS 欄走訪的單一事實源**：組序 → 組內 worker → 其 in-flight task。
///
/// 過濾語意（切片 C）：
/// - worker 自己命中 → 它與**全部**交錯 task 都留（人是照著 worker 名字找的，
///   找到之後把它的任務砍一半沒有道理）
/// - worker 沒命中、但底下有 task 命中 → worker 列**必留**（否則畫面上會有
///   一列沒有 worker 的孤兒 task），只留命中的那幾條
/// - 兩者皆無 → 整個 worker 連同 task 都不畫
/// - 組標頭只在該組還有存活 worker 列時才吐
fn walk_workers(model: &Model, filter: &Filter, mut emit: impl FnMut(WalkEvent)) {
    for (gi, g) in model.groups.iter().enumerate() {
        // 空組不可達：`group_by_lineage` 只在有成員時才建組（lineage 組來自
        // `entry().or_default().push()`，standalone 段有 `is_empty()` 閘）
        debug_assert!(
            !g.members.is_empty(),
            "空組不可達（group_by_lineage 不產生）"
        );
        let mut head_done = false;
        for &wi in &g.members {
            let hit_worker = filter.matches(&model.workers[wi].name);
            let tasks: Vec<usize> = tasks_of(model, wi)
                .filter(|&ti| hit_worker || filter.matches(&task_hay(&model.tasks[ti])))
                .collect();
            if !hit_worker && tasks.is_empty() {
                continue;
            }
            if !head_done {
                emit(WalkEvent::GroupHead(gi));
                head_done = true;
            }
            emit(WalkEvent::Row(Row::Worker(wi)));
            for ti in tasks {
                emit(WalkEvent::Row(Row::Task {
                    worker: wi,
                    task: ti,
                }));
            }
        }
    }
}

/// task 列的比對面：id／from／to／status 串接（欄位間一個空格）。
///
/// 為什麼四欄都進來：人記得的可能是任務 id 的尾碼、也可能是「誰派給誰」或
/// 「還在 running 的那些」。**不含 worker 名字以外的推導**——這裡串的都是
/// task 自己身上的欄位。
pub fn task_hay(t: &InFlight) -> String {
    format!("{} {} {} {}", t.id, t.from, t.to, t.status)
}

/// 選取列在畫面上的行號（列號＋它前面所有組標頭）。
pub fn worker_line_of(model: &Model, filter: &Filter, row_idx: usize) -> usize {
    worker_row_lines(model, filter)
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
pub fn worker_page_row(
    model: &Model,
    filter: &Filter,
    row_idx: usize,
    page_lines: usize,
    down: bool,
) -> usize {
    let lines = worker_row_lines(model, filter);
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
pub fn task_rows(model: &Model, filter: &Filter, scope: Scope) -> Vec<usize> {
    (0..model.recent.len())
        .filter(|&i| filter.matches(&task_hay(&model.recent[i])))
        .filter(|&i| match scope {
            Scope::All => true,
            // Unattached＝**沒有任何一列** registry 證明得了它的歸屬。
            // 注意這是全 registry 的判定，不是「當前這一組」——一個 task 只要
            // 掛得上某一列，它就不是無主的
            Scope::Unattached => {
                !(0..model.workers.len()).any(|wi| attached(&model.recent[i], &model.workers[wi]))
            }
        })
        .collect()
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
            spawned_at: String::new(),
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

    /// 測試用：由 task-id 前綴反推 ISO `created_at`（真實資料裡兩者同源，
    /// **不一致是例外**——那條例外由 `attachment_needs_...` 專門驗）。
    fn iso_from_id(id: &str) -> String {
        let s: Vec<char> = id.chars().take(16).collect();
        if s.len() < 16 {
            return String::new();
        }
        let g = |a: usize, b: usize| -> String { s[a..b].iter().collect() };
        format!(
            "{}-{}-{}T{}:{}:{}Z",
            g(0, 4),
            g(4, 6),
            g(6, 8),
            g(9, 11),
            g(11, 13),
            g(13, 15)
        )
    }

    fn inflight(id: &str, to: &str) -> InFlight {
        InFlight {
            created_at: iso_from_id(id),
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
            worker_rows(&model, &Filter::default()),
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
                created_at: "2026-07-31T00:00:09Z".into(),
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
        let idx = TagIndex::build(&model.workers);
        let aff = |i: usize| worker_affiliation(&model.workers[i], &roots, &idx);
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
        let idx = TagIndex::build(&model.workers);
        let roots: std::collections::HashSet<String> = std::collections::HashSet::new();
        let aff = |i: usize| worker_affiliation(&model.workers[i], &roots, &idx);
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
        let idx = TagIndex::build(&model.workers);
        assert_eq!(
            worker_affiliation(&model.workers[1], &roots, &idx),
            Affiliation::Invalid,
            "兩欄互相矛盾時不猜哪一邊對"
        );
        assert_eq!(model.groups[1].key, GroupKey::Standalone);
        assert_eq!(model.groups[1].members, vec![1], "invalid 列 standalone");
    }

    // ── P4.7 切片 C：filter／scope／掛載判準 ───────────────────────────

    /// 帶 `registered_at` 的一列（切片 C 的掛載判準吃的就是它）
    fn at(mut w: AgentSnapshot, registered_at: &str) -> AgentSnapshot {
        w.registered_at = registered_at.to_string();
        w
    }

    fn task_to(id: &str, to: &str) -> InFlight {
        InFlight {
            created_at: iso_from_id(id),
            id: id.to_string(),
            from: "alice".into(),
            to: to.to_string(),
            status: "running".into(),
        }
    }

    /// **`/` 是 literal，不是 regex**（gate (d)）。metachar 一律當普通字元：
    /// 查 `a.c` 不得命中 `abc`——這是「有沒有偷偷接上 regex 引擎」的判準，
    /// 而不是風格偏好（接上去就得回答不合法 pattern 與災難性回溯怎麼辦）。
    #[test]
    fn the_filter_is_literal_and_case_insensitive() {
        let f = |q: &str| Filter { query: q.into() };
        assert!(f("w1").matches("p4w1x"), "substring");
        assert!(f("W1").matches("p4w1x"), "case-insensitive");
        assert!(f("P4").matches("p4w1x"));
        assert!(Filter::default().matches("anything"), "停用時一律命中");

        // 負向：metachar 逐一驗（`.` `*` `[` `?` ＋常見的其餘幾個）
        assert!(!f("a.c").matches("abc"), "`.` MUST NOT 當萬用字元");
        assert!(!f("a*c").matches("ac"), "`*` MUST NOT 當重複");
        assert!(!f("a*c").matches("abbbc"));
        assert!(!f("[ab]").matches("a"), "`[]` MUST NOT 當字元集");
        assert!(!f("a?c").matches("ac"), "`?` MUST NOT 當可選");
        assert!(!f("a+").matches("aaa"), "`+` MUST NOT 當重複");
        assert!(!f("a|b").matches("a"), "`|` MUST NOT 當交替");
        assert!(!f("^a").matches("abc"), "`^` MUST NOT 當行首");
        assert!(!f("c$").matches("abc"), "`$` MUST NOT 當行尾");
        assert!(!f("(a)").matches("a"), "`()` MUST NOT 當群組");
        assert!(!f("a\\d").matches("a1"), "`\\d` MUST NOT 當字元類");
        // 反過來：metachar 真的出現在字串裡時**必須**命中（它是字面）
        assert!(f("a.c").matches("xa.cx"));
        assert!(f("[ab]").matches("x[ab]y"));
    }

    /// **掛載判準**（切片 C 的單一事實源）：同名 respawn 不承接歷史 task。
    #[test]
    fn attachment_needs_same_name_and_a_later_stamp() {
        let w = at(snap("w1", true, "s:@1"), "2026-07-31T12:00:00Z");

        // (a) 同世代：task 在 registered_at 之後 → 掛
        assert!(attached(&task_to("20260731T130000Z-aaaa", "w1"), &w));
        // **邊界：同一秒＝不可證＝不掛**（修正輪 R2／F1）。這不是筆誤：磁碟
        // 時戳只到整秒，「先註冊後派任務」與「先建立 task 後 respawn 同名
        // worker」在同一秒裡分不出來，而契約禁止的是後者那種 false positive。
        // 代價（register 後同秒派的任務暫時落 Unattached）是可見且可恢復的
        assert!(
            !attached(&task_to("20260731T120000Z-aaaa", "w1"), &w),
            "同秒歧義 MUST fail-closed"
        );
        // (b) 同名 respawn：task 早於這一代的 registered_at → 不掛
        assert!(
            !attached(&task_to("20260731T110000Z-aaaa", "w1"), &w),
            "早於 registered_at 的 task 屬於前一代"
        );
        // (c) 時戳壞（兩側各一種）→ 不可證＝不掛（fail-closed）
        assert!(!attached(&task_to("not-a-stamp-xxxx", "w1"), &w));
        // `created_at` 缺失／壞形狀（修正輪 R1）：欄位讀不到就是空字串
        let mut no_created = task_to("20260731T130000Z-aaaa", "w1");
        no_created.created_at = String::new();
        assert!(!attached(&no_created, &w), "created_at 缺失 → 不掛");
        let mut bad_created = task_to("20260731T130000Z-aaaa", "w1");
        bad_created.created_at = "31/07/2026 13:00".into();
        assert!(!attached(&bad_created, &w), "created_at 壞形狀 → 不掛");

        // **解析是真的解析**（修正輪 R2／F2）：非法曆日、separator 錯位、
        // 長度對但內容不是日期——一律不可證。自己寫的「刪掉 `-` `:` 再數位數」
        // 骨架檢查會把這些全部放行，fail-closed 因此只是看起來成立
        for bad in [
            "2026-00-00T00:00:00Z",      // 月／日為 0
            "2026-99-99T99:99:99Z",      // 全部超界
            "2026-02-31T12:00:00Z",      // 曆日不存在（round-trip 才抓得到）
            "2026:07:31T12-00-00Z",      // separator 位置錯
            "2026-07-31T12:00:00+08:00", // 帶 offset：不是本工具寫得出的形狀
        ] {
            let bad_w = at(snap("w1", true, ""), bad);
            assert!(
                !attached(&task_to("20260731T130000Z-aaaa", "w1"), &bad_w),
                "registered_at 「{bad}」MUST 解析不出 → 不掛"
            );
            let mut bad_t = task_to("20260731T130000Z-aaaa", "w1");
            bad_t.created_at = bad.into();
            assert!(!attached(&bad_t, &w), "created_at 「{bad}」同理");
        }

        // **判準是 `created_at`，不是 task-id 前綴**（修正輪 R1 的回歸鎖）。
        // 真實 fixture 有兩者不一致的（分組 41：id 寫 00:41:41、created_at 寫
        // 04:41:42），拿 id 當證據會把它判成前朝遺物、內嵌 task 列整條消失。
        let mut late_created = task_to("20260731T004141Z-41cc", "w1");
        late_created.created_at = "2026-07-31T13:00:00Z".into();
        assert!(
            attached(&late_created, &w),
            "id 前綴早於註冊、但 created_at 晚於註冊 → **掛**"
        );
        let mut early_created = task_to("20260731T230000Z-zzzz", "w1");
        early_created.created_at = "2026-07-31T09:00:00Z".into();
        assert!(
            !attached(&early_created, &w),
            "id 前綴晚於註冊、但 created_at 早於註冊 → **不掛**"
        );
        assert!(
            !attached(
                &task_to("20260731T130000Z-aaaa", "w1"),
                &at(snap("w1", true, ""), "")
            ),
            "legacy／人工註冊沒有 registered_at → 落 Unattached"
        );
        assert!(!attached(
            &task_to("20260731T130000Z-aaaa", "w1"),
            &at(snap("w1", true, ""), "31/07/2026 12:00")
        ));
        // (d) 名字不同 → 不掛（世代再新也不是它的）
        assert!(!attached(&task_to("20260731T130000Z-aaaa", "w2"), &w));
    }

    /// **task → worker 只在唯一可證時才成立**（修正輪 R2／F3）。
    ///
    /// 0 筆與 >1 筆都回 `None`：前者是無主任務（Unattached 看得到的那些），
    /// 後者是 registry 自相矛盾——兩種都不得靜默認領一個當代同名 worker。
    #[test]
    fn a_task_binds_to_a_worker_only_when_exactly_one_can_claim_it() {
        let cur = at(snap("w1", true, "s:@1"), "2026-07-31T12:00:00Z");
        let model = model_of(vec![cur], Vec::new());
        // 恰一筆
        assert_eq!(
            worker_of_task(&model, &task_to("20260731T130000Z-aaaa", "w1")),
            Some(0)
        );
        // 0 筆：歷史 task（早於這一代註冊）→ 不得認領當代的同名 worker
        assert_eq!(
            worker_of_task(&model, &task_to("20260731T110000Z-aaaa", "w1")),
            None,
            "Unattached 的歷史 task MUST NOT 認領當代 worker"
        );
        // 0 筆：收件人根本不在 registry
        assert_eq!(
            worker_of_task(&model, &task_to("20260731T130000Z-aaaa", "gone")),
            None
        );
        // >1 筆：registry 自相矛盾（同名兩列，兩列都掛得上）→ 不挑贏家
        let dup = model_of(
            vec![
                at(snap("w1", true, "s:@1"), "2026-07-31T10:00:00Z"),
                at(snap("w1", true, "s:@2"), "2026-07-31T11:00:00Z"),
            ],
            Vec::new(),
        );
        assert_eq!(
            worker_of_task(&dup, &task_to("20260731T130000Z-aaaa", "w1")),
            None,
            "同名多列 MUST NOT 依檔序靜默選第一個"
        );
    }

    /// TASKS scope：`Unattached` 只列**沒有任何一列**證明得了歸屬的 task。
    #[test]
    fn the_unattached_scope_lists_only_what_nobody_can_claim() {
        let mut model = model_of(
            vec![
                at(snap("w1", true, "s:@1"), "2026-07-31T12:00:00Z"),
                at(snap("w2", true, "s:@1"), "2026-07-31T12:00:00Z"),
            ],
            Vec::new(),
        );
        model.recent = vec![
            task_to("20260731T130000Z-aaaa", "w1"),   // 掛得上 w1
            task_to("20260731T110000Z-bbbb", "w1"),   // 前一代的 w1 → 無主
            task_to("20260731T130000Z-cccc", "gone"), // 收件人已不在 registry
            task_to("20260731T130000Z-dddd", "w2"),   // 掛得上 w2
        ];
        let all = task_rows(&model, &Filter::default(), Scope::All);
        assert_eq!(all, vec![0, 1, 2, 3], "All＝全 pool（現行語意）");
        assert_eq!(
            task_rows(&model, &Filter::default(), Scope::Unattached),
            vec![1, 2],
            "Unattached＝證不出歸屬的那些"
        );
        // filter 與 scope 是兩個獨立的軸，疊起來要都成立
        assert_eq!(
            task_rows(
                &model,
                &Filter {
                    query: "cccc".into()
                },
                Scope::Unattached
            ),
            vec![2]
        );
        assert_eq!(Scope::All.toggled(), Scope::Unattached);
        assert_eq!(Scope::Unattached.toggled(), Scope::All);
    }

    /// **三處走訪必須逐字一致**（verifier B1 留言）：`worker_rows`／
    /// `worker_row_lines`／`group_line_offsets` 曾各自手抄一份 `t.to == name`，
    /// 同名跨 lineage 時會雙掛，而漂移的症狀是靜默的（選取列被推出畫面外）。
    ///
    /// fixture 刻意含**同名兩列**：兩條 lineage 各有一個叫 `dup` 的 worker，
    /// 舊的那一代註冊得早。這種資料只在 registry 自相矛盾時可達（一名一檔），
    /// 但正是它會讓三處走訪漂開，所以測試要造得出來。`attached()` 至少擋掉
    /// 「歷史 task 上新一代」，其餘照裁定兩列都顯示。
    #[test]
    fn the_three_row_walks_never_drift() {
        let ra = "ra-1-aaaaaaaaaaaa";
        let rb = "rb-1-bbbbbbbbbbbb";
        let old = at(lin("dup", ra, Some(ra), None), "2026-07-31T10:00:00Z");
        let new = at(lin("dup", rb, Some(rb), None), "2026-07-31T20:00:00Z");
        let other = at(
            lin("solo", "s-2-cccccccccccc", Some(ra), Some(ra)),
            "2026-07-31T10:00:00Z",
        );
        let model = model_of(
            vec![old, other, new],
            vec![
                task_to("20260731T110000Z-aaaa", "dup"), // 舊那一代的
                task_to("20260731T210000Z-bbbb", "dup"), // 新那一代的
                task_to("20260731T110000Z-cccc", "solo"),
            ],
        );
        // **filter 開／關兩種狀態各驗一次**（修正輪 R2／F4）：`worker_row_lines`
        // 是 `worker_line_of`（捲動）與 `worker_page_row`（翻頁）唯一的行號
        // 來源，先前兩個呼叫點都只餵 `Filter::default()`，過濾生效下那條路徑
        // 一條斷言都沒有
        for f in [
            Filter::default(),
            Filter {
                query: "dup".into(),
            },
        ] {
            let rows = worker_rows(&model, &f);
            let lines = worker_row_lines(&model, &f);
            let heads = group_line_offsets(&model, &f);
            assert_eq!(rows.len(), lines.len(), "列數必須一致（filter={f:?}）");
            assert!(
                lines.windows(2).all(|w| w[0] < w[1]),
                "行號必須嚴格遞增（filter={f:?}）"
            );
            // P5.3 兩行列：`lines` 記各列**頂行**，最後一列頂行＋其行高＝
            // 總行數（`worker_lines_total` 是捲軌總量的單一來源，兩者必同源）
            assert_eq!(
                lines.last().copied().unwrap_or(0)
                    + rows.last().map(|r| row_height(*r)).unwrap_or(0),
                worker_lines_total(&model, &f),
                "最後一列頂行＋行高＝總行數（filter={f:?}）"
            );
            for (&row, _) in heads.iter() {
                assert!(
                    matches!(rows.get(row), Some(Row::Worker(_))),
                    "標頭 MUST 落在某個 worker 列之前（filter={f:?}, row={row}）"
                );
            }
            // 行號換算的兩個消費者也吃同一份：選中最後一列時，`worker_line_of`
            // 回的必須就是 `lines` 的最後一項
            let last = rows.len() - 1;
            assert_eq!(
                worker_line_of(&model, &f, last),
                lines[last],
                "worker_line_of MUST 與走訪同源（filter={f:?}）"
            );
        }
        let f = Filter::default();
        let rows = worker_rows(&model, &f);
        let lines = worker_row_lines(&model, &f);
        let heads = group_line_offsets(&model, &f);
        assert_eq!(rows.len(), lines.len(), "列數必須一致");

        // **歷史 task 不上新一代**（`attached()` 修掉的那一半）：11:00 那筆只
        // 掛在 10:00 註冊的舊 dup 底下，不會出現在 20:00 註冊的新 dup 底下
        let under = |wi: usize| -> Vec<usize> {
            rows.iter()
                .filter_map(|r| match r {
                    Row::Task { worker, task } if *worker == wi => Some(*task),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(under(2), vec![1], "新一代只收得到自己註冊之後的 task");
        assert!(
            !under(2).contains(&0),
            "同名 respawn MUST NOT 承接歷史 task"
        );
        // **同名多列時兩邊都顯示——這是裁定過的行為，不是缺口**（修正輪 R2）。
        // 21:00 那筆同時晚於兩代的 `registered_at`，逐對判準對兩列都成立。
        //
        // 為什麼不收斂成「取 registered_at 最大的那一列」：`registry::snapshot`
        // 一名一檔（`agents/<name>.json`），正常 respawn 是**同路徑覆寫**，
        // 只會有一列。同名兩列只在 registry 被手改成自相矛盾時可達——在那種
        // 資料上挑一個贏家，等於從壞資料裡靜默選邊；兩列都顯示，人才看得出
        // registry 有問題
        assert_eq!(
            under(0),
            vec![0, 1],
            "同名多列時 MUST 兩列都顯示，不靜默挑一個"
        );
        // 行號嚴格遞增，且每一組標頭恰佔一行（P5.3：總量以行高計）
        assert!(lines.windows(2).all(|w| w[0] < w[1]), "行號必須嚴格遞增");
        assert_eq!(
            lines.last().copied().unwrap_or(0) + rows.last().map(|r| row_height(*r)).unwrap_or(0),
            worker_lines_total(&model, &f),
            "最後一列頂行＋行高＝總行數"
        );
        // 標頭落點必須是某一列的位置，且該列是 worker 列（組的第一個成員）
        for (&row, _) in heads.iter() {
            assert!(
                matches!(rows.get(row), Some(Row::Worker(_))),
                "標頭 MUST 落在某個 worker 列之前（row={row}）"
            );
        }
    }

    /// filter 的連動語意：worker 命中→其 task 全留；task 命中→其 worker 必留；
    /// 都沒命中→整個 worker 消失；組標頭只在該組還有列時才畫。
    #[test]
    fn filtering_keeps_workers_and_their_tasks_together() {
        let root = "root-1-aaaaaaaaaaaa";
        let model = model_of(
            vec![
                at(lin("alpha", root, Some(root), None), "2026-07-31T00:00:00Z"),
                at(
                    lin("beta", "b-2-bbbbbbbbbbbb", Some(root), Some(root)),
                    "2026-07-31T00:00:00Z",
                ),
                at(manual("gamma"), "2026-07-31T00:00:00Z"),
            ],
            vec![
                task_to("20260731T010000Z-aaaa", "alpha"),
                task_to("20260731T010000Z-bbbb", "alpha"),
                task_to("20260731T010000Z-cccc", "beta"),
            ],
        );
        // (a) worker 命中 → 它的兩筆 task 都留
        let rows = worker_rows(
            &model,
            &Filter {
                query: "alpha".into(),
            },
        );
        assert_eq!(
            rows,
            vec![
                Row::Worker(0),
                Row::Task { worker: 0, task: 0 },
                Row::Task { worker: 0, task: 1 },
            ]
        );
        // standalone 組整組消失 → 它的標頭也不畫
        let heads = group_line_offsets(
            &model,
            &Filter {
                query: "alpha".into(),
            },
        );
        assert_eq!(heads.len(), 1, "只剩一組有列，就只有一個標頭");
        // (b) task 命中 → 它的 worker 列必留（不得有孤兒 task 列），
        //     且**只留命中的那一筆**
        let rows = worker_rows(
            &model,
            &Filter {
                query: "cccc".into(),
            },
        );
        assert_eq!(rows, vec![Row::Worker(1), Row::Task { worker: 1, task: 2 }]);
        // (c) 都沒命中 → 空（DETAIL 端由 `Sel::None` 接手，見 app 層測試）
        assert!(
            worker_rows(
                &model,
                &Filter {
                    query: "zzz".into()
                }
            )
            .is_empty()
        );
        assert!(
            group_line_offsets(
                &model,
                &Filter {
                    query: "zzz".into()
                }
            )
            .is_empty()
        );
        // (d) 標頭的括號數字算**畫得出來的**那些
        let vis = group_visible_members(
            &model,
            &Filter {
                query: "beta".into(),
            },
        );
        assert_eq!(
            vis.get(&0).copied(),
            Some(1),
            "組有兩員，畫得出來的只有一員"
        );
    }

    // ── DETAIL breadcrumb（P4.7 切片 B2）＋B6 防護矩陣 ──────────────────
    //
    // 主力放這一層：breadcrumb 是純函式，render 只投影（view.rs 抽樣驗字面）。

    /// 共用的四代鏈 root→A→B→C。tag 的 name 段刻意用大寫，墓碑字面才與
    /// gate (a) 的 `B†` 逐字對得上（`is_generation_key` 的 name 段允許大寫，
    /// 只有 12 位 hex 段禁止）。
    const T_ROOT: &str = "root-1-aaaaaaaaaaaa";
    const T_A: &str = "A-2-bbbbbbbbbbbb";
    const T_B: &str = "B-3-cccccccccccc";
    const T_C: &str = "C-4-dddddddddddd";

    /// 某一列的 breadcrumb 畫面字串（`None`＝這一列沒有說得出口的世代）
    fn bc(model: &Model, name: &str) -> Option<String> {
        let w = model
            .workers
            .iter()
            .find(|w| w.name == name)
            .expect("fixture 裡沒有這個 worker");
        breadcrumb(model, w).map(|c| breadcrumb_line(&c))
    }

    /// **gate (a) render 面的字面契約**：root→A→B→C 移除 A／B 之後，C 的
    /// breadcrumb ＝ `root → … → B† → C`——A 無資料成省略號、B 留墓碑。
    ///
    /// 三件事同時被釘住：省略號的語意（「這中間還有幾代，說不出是誰」）、
    /// 墓碑只在 display 出現（`B† (cccc)` 的 name 段來自 **tag 剖析**，
    /// 不是拿名字查表）、以及 root 在場時用它的 agent 名。
    #[test]
    fn gate_a_breadcrumb_reads_root_gap_tombstone_self() {
        let model = model_of(
            vec![
                lin("root", T_ROOT, Some(T_ROOT), None),
                lin("C", T_C, Some(T_ROOT), Some(T_B)),
                // **同名誘惑**：畫面上真的有一個 agent 叫 `B`，但它是另一代
                // （另一個 spawn_tag）。以名字查表的實作會在這裡把墓碑換成
                // 這個活人——每一跳只比對 generation key 全串才不會上當
                AgentSnapshot {
                    lineage_root: None,
                    parent_agent: None,
                    ..lin("B", "decoy-9-999999999999", None, None)
                },
            ],
            Vec::new(),
        );
        assert_eq!(
            bc(&model, "C").as_deref(),
            Some("root \u{2192} \u{2026} \u{2192} B\u{2020} (cccc) \u{2192} C"),
            "gate (a)：root → … → B† → C（同名的在場者不得冒充那一節）"
        );
        // 節點層的形狀（畫面字串之外，render 還要知道哪一節是墓碑）
        let w = &model.workers[1];
        let crumbs = breadcrumb(&model, w).expect("C 是合法的 lineage 列");
        assert_eq!(crumbs.len(), 4);
        assert_eq!(crumbs[1], Crumb::Gap, "root 與 parent 之間是斷層");
        assert!(
            matches!(&crumbs[0], Crumb::Node { tombstone, .. } if !tombstone),
            "root 在場：不是墓碑"
        );
        assert!(
            matches!(&crumbs[2], Crumb::Node { tombstone, .. } if *tombstone),
            "parent 缺席：墓碑"
        );

        // **負向：MUST NOT 由 task 推導**。加一筆 `from` 恰好叫 "B"（甚至
        // 叫 "A"）的 task，等於把「誰跟誰講過話」擺在最誘人的位置
        // （`InFlight` 沒有 `Clone`，兩欄各寫一份——不為了測試動 ab-core 的
        // 公開型別，同 writer5 對 `AgentSnapshot` 的權衡）
        let temptation = || InFlight {
            created_at: "2026-07-31T00:00:09Z".into(),
            id: "20260731T000009Z-zzzz".into(),
            from: "B".into(),
            to: "C".into(),
            status: "queued".into(),
        };
        let mut tempted = model_of(
            vec![
                lin("root", T_ROOT, Some(T_ROOT), None),
                lin("C", T_C, Some(T_ROOT), Some(T_B)),
            ],
            vec![temptation()],
        );
        tempted.recent = vec![temptation()];
        assert_eq!(
            bc(&tempted, "C"),
            bc(&model, "C"),
            "task 的 from/to 不參與 breadcrumb（在場的 B 也不會因此變成非墓碑）"
        );
    }

    /// 節點數的三種形狀：self 即 root（單節點）／parent 即 root（兩節點、
    /// **無**省略號）／祖先全在場（走得完就走完，不無中生有一個斷層）。
    #[test]
    fn breadcrumb_shapes_follow_what_the_registry_can_prove() {
        // self 即 root
        let solo = model_of(vec![lin("root", T_ROOT, Some(T_ROOT), None)], Vec::new());
        assert_eq!(bc(&solo, "root").as_deref(), Some("root"));

        // parent 即 root
        let two = model_of(
            vec![
                lin("root", T_ROOT, Some(T_ROOT), None),
                lin("A", T_A, Some(T_ROOT), Some(T_ROOT)),
            ],
            Vec::new(),
        );
        assert_eq!(bc(&two, "A").as_deref(), Some("root \u{2192} A"));

        // 四代全在場：每一跳都有證據，於是一節省略號都沒有
        let full = model_of(
            vec![
                lin("root", T_ROOT, Some(T_ROOT), None),
                lin("A", T_A, Some(T_ROOT), Some(T_ROOT)),
                lin("B", T_B, Some(T_ROOT), Some(T_A)),
                lin("C", T_C, Some(T_ROOT), Some(T_B)),
            ],
            Vec::new(),
        );
        assert_eq!(
            bc(&full, "C").as_deref(),
            Some("root \u{2192} A \u{2192} B \u{2192} C")
        );
        assert_eq!(
            bc(&full, "B").as_deref(),
            Some("root \u{2192} A \u{2192} B")
        );
    }

    /// **root 缺席降級**：連 root 的 registry 都不在了，它也只能是墓碑——
    /// 而墓碑仍然說得出「這條 lineage 是哪一條」（generation key 的短碼）。
    #[test]
    fn an_absent_root_degrades_to_a_tombstone_not_to_silence() {
        let model = model_of(vec![lin("C", T_C, Some(T_ROOT), Some(T_B))], Vec::new());
        assert_eq!(
            bc(&model, "C").as_deref(),
            Some("root\u{2020} (aaaa) \u{2192} \u{2026} \u{2192} B\u{2020} (cccc) \u{2192} C")
        );

        // parent 即 root 且 root 缺席：兩節點、**無**省略號（沒有中間世代
        // 可言，畫一個斷層等於宣稱有一段不存在的血緣）
        let two = model_of(vec![lin("A", T_A, Some(T_ROOT), Some(T_ROOT))], Vec::new());
        assert_eq!(
            bc(&two, "A").as_deref(),
            Some("root\u{2020} (aaaa) \u{2192} A")
        );

        // 剖析不出來的 key（合文法但短碼取不到）不編故事。這裡直接驗
        // `crumb_of` 的退路字面：`?†`
        let idx = TagIndex::build(&[]);
        assert_eq!(
            crumb_of("not-a-generation-key", &idx),
            Crumb::Node {
                label: "?\u{2020}".to_string(),
                tombstone: true
            }
        );
    }

    /// **B6：cycle**。兩列互為 parent（B↔C，同一個 root），traversal MUST
    /// 停得下來且 MUST NOT 讓同一節出現兩次。
    ///
    /// mutation 證據：拔掉 `visited` 這道防護，走法會變成 C→B→C→B…，由
    /// `hop_limit` 截斷，字串長出重複節點 → 本測試紅。
    #[test]
    fn a_cycle_cannot_walk_forever() {
        let model = model_of(
            vec![
                lin("root", T_ROOT, Some(T_ROOT), None),
                lin("B", T_B, Some(T_ROOT), Some(T_C)),
                lin("C", T_C, Some(T_ROOT), Some(T_B)),
            ],
            Vec::new(),
        );
        let line = bc(&model, "C").expect("形狀合法（cycle 的傷害在 traversal）");
        assert_eq!(
            line, "root \u{2192} \u{2026} \u{2192} B \u{2192} C",
            "繞回走過的節點就停，斷掉的地方誠實補省略號"
        );
        assert_eq!(line.matches(" C").count(), 1, "同一節不得出現兩次");

        // **自指的另一種形狀**：一列宣稱自己是 root，卻又寫著 parent。
        // 修正輪 H1.1 起這是**矛盾形**（根的定義就是沒有 parent），整列
        // Invalid → 沒有 breadcrumb。
        //
        // 前一版把它鎖成「單節點 A」——那等於用畫面掩蓋一列自相矛盾的
        // registry，是錯誤行為被斷言鎖定，不是保護
        let self_root = model_of(
            vec![
                lin("A", T_ROOT, Some(T_ROOT), Some(T_B)),
                lin("B", T_B, Some(T_ROOT), Some(T_ROOT)),
            ],
            Vec::new(),
        );
        assert_eq!(bc(&self_root, "A"), None, "自稱根卻有 parent＝矛盾形");
        // B 的 root 指向 A 的 tag、parent 也是 A——但 A 這一列 invalid，
        // 走到它就停（在場，所以是名字不是墓碑）
        assert_eq!(bc(&self_root, "B").as_deref(), Some("A \u{2192} B"));
    }

    /// **H1（修正輪）：三個先前放行的漏形**。三者都會讓一列自相矛盾的
    /// registry 混進歸屬判定，且畫面上看起來完全正常。
    #[test]
    fn contradictory_rows_are_invalid_not_lineage() {
        // (1) 自稱根卻有 parent
        let self_root = lin("self-root", T_ROOT, Some(T_ROOT), Some(T_B));
        // (2) 有 spawn_tag、卻不是 spawn 出來的（寫入路徑產不出這種列）
        let not_spawned = AgentSnapshot {
            spawned: false,
            ..lin("not-spawned", T_A, Some(T_ROOT), Some(T_ROOT))
        };
        // (3) parent 是 legacy（說不出自己的 lineage），子代卻宣稱屬於別的 R
        //     ——切片 A 的 fallback 是「退 parent 自身 spawn_tag」，所以合法的
        //     子代只能宣稱 root ＝ parent 的 tag
        let legacy_parent = AgentSnapshot {
            lineage_root: None,
            parent_agent: None,
            ..lin("legacy-parent", T_B, None, None)
        };
        let false_claim = lin("false-claim", T_C, Some(T_ROOT), Some(T_B));
        let model = model_of(
            vec![
                lin("root", T_ROOT, Some(T_ROOT), None),
                self_root,
                not_spawned,
                legacy_parent,
                false_claim,
            ],
            Vec::new(),
        );
        let roots: std::collections::HashSet<String> =
            std::collections::HashSet::from([canon(T_ROOT)]);
        let idx = TagIndex::build(&model.workers);
        let aff = |i: usize| worker_affiliation(&model.workers[i], &roots, &idx);
        assert_eq!(aff(1), Affiliation::Invalid, "自稱根卻有 parent");
        assert_eq!(
            aff(2),
            Affiliation::Invalid,
            "spawn_tag 在、spawned 不是 true"
        );
        assert_eq!(
            aff(4),
            Affiliation::Invalid,
            "legacy parent 之下的假 root 宣稱"
        );
        for name in ["self-root", "not-spawned", "false-claim"] {
            assert_eq!(bc(&model, name), None, "{name} MUST NOT 有 breadcrumb");
        }

        // 對照組：同一個 legacy parent 之下，root ＝ parent 自身 tag 就合法
        let ok = model_of(
            vec![
                AgentSnapshot {
                    lineage_root: None,
                    parent_agent: None,
                    ..lin("legacy-parent", T_B, None, None)
                },
                lin("kid", T_C, Some(T_B), Some(T_B)),
            ],
            Vec::new(),
        );
        assert_eq!(
            bc(&ok, "kid").as_deref(),
            Some("legacy-parent \u{2192} kid"),
            "legacy parent ＝這一條 lineage 的根（切片 A 的 fallback 語意）"
        );
    }

    /// **H2（修正輪）：`spawn_tag` 重複＝身分不可證**。
    ///
    /// HashMap 靜默保留最後寫入的那一列，等於**用目錄序決定身分**——切片 A
    /// 對 ambiguous parent 已經裁過同一件事（不任選）。兩件事要成立：
    /// 重複的列自己一律 Invalid；別人指向它時只能得到墓碑。
    #[test]
    fn a_duplicated_spawn_tag_proves_nothing() {
        let other = "other-9-999999999999";
        // P1／P2 同 tag、不同 root：誰也不能代表這個世代
        let dup_a = lin("dup-a", T_B, Some(T_B), None);
        let dup_b = lin("dup-b", T_B, Some(other), Some(other));
        let child = lin("C", T_C, Some(T_ROOT), Some(T_B));
        let model = model_of(
            vec![lin("root", T_ROOT, Some(T_ROOT), None), dup_a, dup_b, child],
            Vec::new(),
        );
        let roots: std::collections::HashSet<String> =
            std::collections::HashSet::from([canon(T_ROOT)]);
        let idx = TagIndex::build(&model.workers);
        assert_eq!(
            worker_affiliation(&model.workers[1], &roots, &idx),
            Affiliation::Invalid,
            "重複 tag 的列自己也證不出身分"
        );
        assert_eq!(
            worker_affiliation(&model.workers[2], &roots, &idx),
            Affiliation::Invalid
        );
        // C 指向重複的 P → 墓碑（不得拿任一列當在場證據）
        assert_eq!(
            bc(&model, "C").as_deref(),
            Some("root \u{2192} \u{2026} \u{2192} B\u{2020} (cccc) \u{2192} C")
        );

        // **換檔名序結果不變**：兩列對調之後每一條斷言逐字相同。靜默任選的
        // 實作會在這裡分岔（它取到的是另一列）
        let swapped = model_of(
            vec![
                lin("root", T_ROOT, Some(T_ROOT), None),
                lin("dup-b", T_B, Some(other), Some(other)),
                lin("dup-a", T_B, Some(T_B), None),
                lin("C", T_C, Some(T_ROOT), Some(T_B)),
            ],
            Vec::new(),
        );
        let idx2 = TagIndex::build(&swapped.workers);
        assert_eq!(
            worker_affiliation(&swapped.workers[1], &roots, &idx2),
            Affiliation::Invalid
        );
        assert_eq!(
            worker_affiliation(&swapped.workers[2], &roots, &idx2),
            Affiliation::Invalid
        );
        assert_eq!(bc(&swapped, "C"), bc(&model, "C"), "答案不依目錄序");
        assert_eq!(swapped.groups, model.groups, "分組也不依目錄序");
    }

    /// **H3（修正輪）：寬度上限內的 breadcrumb**。底條模式一換行就會把等價
    /// CLI 原文推出畫面，所以過長時收縮成「root → … → self」，再不夠就硬截。
    #[test]
    fn a_breadcrumb_collapses_before_it_wraps() {
        let crumbs = vec![
            Crumb::Node {
                label: "root".into(),
                tombstone: false,
            },
            Crumb::Node {
                label: "middle-one".into(),
                tombstone: false,
            },
            Crumb::Node {
                label: "middle-two".into(),
                tombstone: false,
            },
            Crumb::Node {
                label: "self".into(),
                tombstone: false,
            },
        ];
        let full = "root \u{2192} middle-one \u{2192} middle-two \u{2192} self";
        assert_eq!(breadcrumb_line(&crumbs), full);
        assert_eq!(breadcrumb_line_fit(&crumbs, 80), full, "放得下就原樣");
        assert_eq!(
            breadcrumb_line_fit(&crumbs, 20),
            "root \u{2192} \u{2026} \u{2192} self",
            "放不下 → 只留兩端"
        );
        // 連兩端都放不下：硬截（寧可截也不換行）
        let fit = breadcrumb_line_fit(&crumbs, 8);
        assert_eq!(fit.chars().count(), 8);
        assert!(fit.ends_with('\u{2026}'));
        // 單節點沒有「中間」可收縮，直接走硬截那一支
        let solo = vec![Crumb::Node {
            label: "a-very-long-single-node".into(),
            tombstone: false,
        }];
        assert_eq!(breadcrumb_line_fit(&solo, 6), "a-ver\u{2026}");
    }

    /// **B6：超長鏈**。八代全在場時走得完（hop 上限不得誤殺合法的長鏈），
    /// 且節點序是 root→…→self。
    #[test]
    fn a_long_chain_walks_all_the_way_up() {
        let tags: Vec<String> = (0..8).map(|i| format!("g{i}-{}-{i:012x}", i + 1)).collect();
        let workers: Vec<AgentSnapshot> = (0..8)
            .map(|i| {
                lin(
                    &format!("g{i}"),
                    &tags[i],
                    Some(&tags[0]),
                    if i == 0 { None } else { Some(&tags[i - 1]) },
                )
            })
            .collect();
        let model = model_of(workers, Vec::new());
        assert_eq!(
            bc(&model, "g7").as_deref(),
            Some(
                "g0 \u{2192} g1 \u{2192} g2 \u{2192} g3 \u{2192} g4 \u{2192} g5 \u{2192} g6 \u{2192} g7"
            )
        );
        // hop 上限＝列數＋1：八代八列，剛好走得完（＋1 留給最後一個缺席的
        // 祖先當墓碑，見 `hop_limit`）
        assert_eq!(hop_limit(&model), 9);
    }

    /// **B6：四型與矛盾列沒有 breadcrumb**。legacy／manual／invalid 身上
    /// 沒有說得出口的世代；`parent 在場但 root 不一致`／自指同理（形狀驗證
    /// 就把它們擋在 traversal 之外）。
    #[test]
    fn rows_without_a_provable_generation_have_no_breadcrumb() {
        let other_root = "other-9-999999999999";
        let model = model_of(
            vec![
                // legacy：兩欄缺席
                AgentSnapshot {
                    lineage_root: None,
                    parent_agent: None,
                    ..lin("legacy", T_A, None, None)
                },
                manual("manual"),
                // invalid：自己是自己的 parent
                lin("self-parent", T_B, Some(T_ROOT), Some(T_B)),
                // invalid：parent 在場，但兩人的 root 對不上
                lin("P", other_root, Some(other_root), None),
                lin("cross", T_C, Some(T_ROOT), Some(other_root)),
                // invalid：registry 損壞
                AgentSnapshot {
                    corrupt: true,
                    ..lin("broken", "bk-7-777777777777", Some(T_ROOT), Some(T_ROOT))
                },
            ],
            Vec::new(),
        );
        for name in ["legacy", "manual", "self-parent", "cross", "broken"] {
            assert_eq!(bc(&model, name), None, "{name} 不該畫出 breadcrumb");
        }
        // 對照組：同一份快照裡形狀合法的那一列照樣有 breadcrumb
        assert_eq!(bc(&model, "P").as_deref(), Some("P"));
    }

    /// **B6：半殘的中繼列不得把線拉長**。C 的 parent（X）在場，但 X 自己是
    /// 一列 invalid（root 指向別處）——走到它就停，斷層誠實補省略號。
    ///
    /// 這條擋的是「祖先在場」與「祖先說得通」被當成同一件事：前者只證明有
    /// 一列同名 tag 的 registry，後者才是把它接進這條 lineage 的理由。
    #[test]
    fn a_broken_intermediate_stops_the_walk() {
        let model = model_of(
            vec![
                lin("root", T_ROOT, Some(T_ROOT), None),
                // X 在場、`lineage_root` 也對得上（所以 C 自己仍合法），但它
                // **憑空認領**：說自己屬於 root 這一組，卻不說自己是誰的子代
                lin("X", T_B, Some(T_ROOT), None),
                lin("C", T_C, Some(T_ROOT), Some(T_B)),
            ],
            Vec::new(),
        );
        assert_eq!(
            bc(&model, "C").as_deref(),
            Some("root \u{2192} \u{2026} \u{2192} X \u{2192} C"),
            "走到說不通的祖先就停（它在場，所以是名字不是墓碑）"
        );
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
        assert_eq!(
            task_rows(&model, &Filter::default(), Scope::All),
            vec![0, 1]
        );
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
                blocker_probe(&tmux, "%1").0,
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
        let idx = BlockerIndex::query_with_snippets(&tmux, &["%1".to_string(), String::new()]).0;
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

    // ===== P5.3-1：fmt_elapsed／worker_activity／Summaries =====

    /// 格式邊界逐條：`<60s`／`<1h`／`<24h`／天；解析失敗與時鐘倒退＝`-`。
    #[test]
    fn fmt_elapsed_boundaries() {
        let base = "2026-08-01T00:00:00Z";
        let t0 = ab_core::time::parse_iso_to_epoch(base).unwrap();
        assert_eq!(fmt_elapsed(base, t0), "0s");
        assert_eq!(fmt_elapsed(base, t0 + 59), "59s");
        assert_eq!(fmt_elapsed(base, t0 + 60), "1m00s");
        assert_eq!(fmt_elapsed(base, t0 + 12 * 60 + 40), "12m40s");
        assert_eq!(fmt_elapsed(base, t0 + 3599), "59m59s");
        assert_eq!(fmt_elapsed(base, t0 + 3600), "1h00m");
        assert_eq!(fmt_elapsed(base, t0 + 86_399), "23h59m");
        assert_eq!(fmt_elapsed(base, t0 + 86_400), "1d");
        assert_eq!(fmt_elapsed("not-a-date", t0), "-");
        assert_eq!(fmt_elapsed(base, t0 - 1), "-", "時鐘倒退不得顯示負數");
    }

    /// 忙碌態：同 worker 多筆 in-flight 取 id 最大＝最新。
    #[test]
    fn worker_activity_picks_newest_inflight() {
        let model = model_of(
            vec![snap("w1", true, "it:@1")],
            vec![
                inflight("20260731T000001Z-aaaa", "w1"),
                inflight("20260731T000005Z-zzzz", "w1"),
                inflight("20260731T000003Z-cccc", "w1"),
            ],
        );
        match worker_activity(&model, 0) {
            Activity::Current(ti) => assert_eq!(model.tasks[ti].id, "20260731T000005Z-zzzz"),
            other => panic!("預期 Current，得到 {other:?}"),
        }
    }

    /// 閒置基準取 `max(最近 attached recent 的 created_at, spawned_at)`：
    /// 兩個方向各驗一次；兩者皆缺＝None；`last` 指向 recent 裡最新 attached。
    #[test]
    fn worker_activity_idle_basis_is_max_of_last_task_and_spawn() {
        // recent 較晚：基準＝task created_at
        let mut m = model_of(
            vec![AgentSnapshot {
                spawned_at: "2026-07-31T00:00:30Z".to_string(),
                ..snap("w1", true, "it:@1")
            }],
            vec![],
        );
        m.recent = vec![
            inflight("20260731T000200Z-bbbb", "w1"), // 最新（反序在前）
            inflight("20260731T000100Z-aaaa", "w1"),
        ];
        match worker_activity(&m, 0) {
            Activity::Idle { since, last } => {
                assert_eq!(
                    since,
                    ab_core::time::parse_iso_to_epoch("2026-07-31T00:02:00Z")
                );
                assert_eq!(last, Some(0));
            }
            other => panic!("預期 Idle，得到 {other:?}"),
        }
        // spawn 較晚（respawn 後還沒接過任務的形狀）：基準＝spawned_at
        let mut m2 = model_of(
            vec![AgentSnapshot {
                spawned_at: "2026-07-31T00:05:00Z".to_string(),
                ..snap("w1", true, "it:@1")
            }],
            vec![],
        );
        m2.recent = vec![inflight("20260731T000200Z-bbbb", "w1")];
        match worker_activity(&m2, 0) {
            Activity::Idle { since, .. } => assert_eq!(
                since,
                ab_core::time::parse_iso_to_epoch("2026-07-31T00:05:00Z")
            ),
            other => panic!("預期 Idle，得到 {other:?}"),
        }
        // 兩者皆解析不出＝None（fail-closed）
        let m3 = model_of(vec![snap("w1", true, "it:@1")], vec![]);
        match worker_activity(&m3, 0) {
            Activity::Idle { since, last } => {
                assert_eq!(since, None);
                assert_eq!(last, None);
            }
            other => panic!("預期 Idle，得到 {other:?}"),
        }
    }

    /// Summaries 快取契約：首行**絕不重讀**（send 後 request.md 不再變，變了
    /// 也照舊——快取值是權威）；events.log 以檔長變更重讀；集合外 id 剔除；
    /// 控制字元在 ingest 淨化。
    #[test]
    fn summaries_cache_never_rereads_first_line_and_tracks_events_by_len() {
        let root = std::env::temp_dir().join(format!(
            "ab-tui-summaries-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths {
            data_dir: root.clone(),
            agents_dir: root.join("agents"),
            tasks_dir: root.join("tasks"),
            locks_dir: root.join("locks"),
            state_dir: root.join("state"),
        };
        paths.ensure_dirs().unwrap();
        let id = "20260731T000001Z-aaaa";
        let dir = paths.tasks_dir.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("request.md"), b"first\tline\nrest\n").unwrap();
        std::fs::write(dir.join("events.log"), b"2026-07-31T00:00:01Z created\n").unwrap();

        let mut model = model_of(vec![], vec![inflight(id, "w1")]);
        let mut sums = Summaries::default();
        sums.sync(&paths, &model);
        assert_eq!(sums.first_line(id), "first line", "控制字元未淨化");
        assert!(sums.last_event(id).contains("created"));

        // 首行：磁碟改了也不重讀（id 不變 → 快取值是權威）
        std::fs::write(dir.join("request.md"), b"REWRITTEN\n").unwrap();
        // events.log：append 改變檔長 → 重讀
        let mut ev = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("events.log"))
            .unwrap();
        std::io::Write::write_all(&mut ev, b"2026-07-31T00:00:02Z delivered\n").unwrap();
        drop(ev);
        sums.sync(&paths, &model);
        assert_eq!(sums.first_line(id), "first line", "首行不該被重讀");
        assert!(
            sums.last_event(id).contains("delivered"),
            "events.log 變長未重讀：{}",
            sums.last_event(id)
        );

        // 集合外剔除：model 不再含這個 id → 快取清空
        model.tasks.clear();
        sums.sync(&paths, &model);
        assert_eq!(sums.first_line(id), "");
        assert_eq!(sums.last_event(id), "");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- P5.4：triage 組間浮頂 ----

    /// 給定 pane→blocker 的索引（其餘 pane 皆 `None`＝查得到、沒 blocker）。
    fn blockers_of(pairs: &[(&str, Blocker)]) -> BlockerIndex {
        BlockerIndex {
            panes: Some(pairs.iter().map(|(p, b)| (p.to_string(), *b)).collect()),
        }
    }

    /// 給定「活著的 pane 集合」的死活索引。
    fn live_of(panes: &[&str]) -> LiveIndex {
        LiveIndex {
            panes: Some(
                panes
                    .iter()
                    .map(|p| (p.to_string(), vec![("s".to_string(), "@1".to_string())]))
                    .collect(),
            ),
            ..LiveIndex::unknown()
        }
    }

    /// 組序 → 組標籤序（斷言讀起來像畫面）。
    fn group_order(m: &Model) -> Vec<String> {
        m.groups.iter().map(|g| group_label(m, g)).collect()
    }

    /// 三條 lineage：`aaa`（正常）／`bbb`（有人卡在權限框）／`ccc`（有人死了）。
    /// pane 由 `snap` 依名字長度決定，這裡名字等長，故顯式改寫。
    fn triage_model() -> Model {
        let mut ws = vec![
            lin("a1", "aaa-1-aaaaaaaaaaaa", Some("aaa-1-aaaaaaaaaaaa"), None),
            lin("b1", "bbb-1-bbbbbbbbbbbb", Some("bbb-1-bbbbbbbbbbbb"), None),
            lin("c1", "ccc-1-cccccccccccc", Some("ccc-1-cccccccccccc"), None),
        ];
        for (i, w) in ws.iter_mut().enumerate() {
            w.pane = format!("%{}", i + 1);
        }
        model_of(ws, Vec::new())
    }

    /// **組間浮頂、組內不排**（裁定 2）：severity 高的組整組上移，組內順序與
    /// 同分組間的相對序一律不動。
    #[test]
    fn triage_floats_groups_by_severity_and_keeps_everything_else_stable() {
        let mut m = triage_model();
        let before = group_order(&m);
        assert_eq!(before.len(), 3, "前提：三組");

        // %2 卡在權限框、%3 死了 → bbb 最上、ccc 次之、aaa 墊底
        let live = live_of(&["%1", "%2"]);
        let bl = blockers_of(&[("%2", Blocker::Prompt)]);
        apply_triage(&mut m, &live, &bl);
        let after = group_order(&m);
        assert_eq!(
            (after[0].contains("b1"), after[1].contains("c1")),
            (true, true),
            "Blocked > Dead > None：實際 {after:?}"
        );

        // 同 severity 維持字典序（stable）：全部無異常時順序 MUST 逐字回到原樣
        let mut m2 = triage_model();
        apply_triage(&mut m2, &live_of(&["%1", "%2", "%3"]), &blockers_of(&[]));
        assert_eq!(group_order(&m2), before, "同分不得重排");

        // 冪等：同一份事實再排一次不動
        let mut m3 = m;
        apply_triage(&mut m3, &live, &bl);
        assert_eq!(group_order(&m3), after, "重排 MUST 冪等");
    }

    /// **severity 消失後組序要回得來**（跨廠複核 2026-08-05 finding 5）。
    ///
    /// 失效形態：對「已經排過的組」再 stable sort，同分時保留的是上一輪的
    /// 浮頂序——blocker 降旗之後 bbb 仍掛在最上面，而畫面上一件事實都沒有
    /// 支撐那個排名。這條從**已排序狀態**起跑，正是上一版驗不到的那一格。
    #[test]
    fn triage_returns_to_the_canonical_order_once_the_severity_clears() {
        let mut m = triage_model();
        let canonical = group_order(&m);

        // 先浮頂
        apply_triage(
            &mut m,
            &live_of(&["%1", "%2", "%3"]),
            &blockers_of(&[("%2", Blocker::Prompt)]),
        );
        assert!(group_order(&m)[0].contains("b1"), "前提：已經浮頂");

        // 降旗（同一份 model，接著排）→ MUST 回到 canonical 序
        apply_triage(
            &mut m,
            &live_of(&["%1", "%2", "%3"]),
            &blockers_of(&[("%2", Blocker::None)]),
        );
        assert_eq!(
            group_order(&m),
            canonical,
            "severity 清除後 MUST 回到 canonical 序，不得留著上一輪的排名"
        );

        // 換一個組出事：排序只跟當下事實有關，與排過幾次無關
        apply_triage(
            &mut m,
            &live_of(&["%1", "%2", "%3"]),
            &blockers_of(&[("%3", Blocker::Prompt)]),
        );
        assert!(group_order(&m)[0].contains("c1"), "改由 c1 浮頂");
    }

    /// **unknown 不浮頂**（§5 三態）：tmux 停擺時整份索引是 unknown，若當成
    /// dead 就會整批浮上來——排序等於作廢，而畫面上一件事實都沒有變。
    #[test]
    fn an_unknown_axis_never_floats_anything() {
        let mut m = triage_model();
        let before = group_order(&m);
        apply_triage(&mut m, &LiveIndex::unknown(), &BlockerIndex::unknown());
        assert_eq!(group_order(&m), before, "unknown MUST 維持中性序");
        // 從**已浮頂**的狀態降級成 unknown 也要回中性序（stale 那一幀走的
        // 正是這條路：畫面已改說 unknown，排序不得繼續替舊事實背書）
        let mut m2 = triage_model();
        apply_triage(
            &mut m2,
            &live_of(&["%1", "%3"]),
            &blockers_of(&[("%2", Blocker::Prompt)]),
        );
        assert!(!group_order(&m2)[0].contains("a1"), "前提：已經浮頂");
        apply_triage(&mut m2, &LiveIndex::unknown(), &BlockerIndex::unknown());
        assert_eq!(group_order(&m2), before, "降級後 MUST 回中性序");
        for wi in 0..m.workers.len() {
            assert_eq!(
                worker_severity(&m, wi, &LiveIndex::unknown(), &BlockerIndex::unknown()),
                Severity::None,
                "unknown 不是異常"
            );
        }
    }

    /// severity 的三個判準各自獨立且定序（`Blocked > Dead > Failed > None`）。
    #[test]
    fn severity_ranks_blocked_above_dead_above_failed() {
        let mut m = triage_model();
        // c1 的最近一輪任務收在 failed（純磁碟軸，與 tmux 無關）
        m.recent = vec![InFlight {
            id: "20260801T000000Z-zzzz".to_string(),
            created_at: "2026-08-01T00:00:00Z".to_string(),
            from: "boss".to_string(),
            to: "c1".to_string(),
            status: "failed".to_string(),
        }];
        let live = live_of(&["%1", "%2", "%3"]);
        assert_eq!(
            worker_severity(&m, 2, &live, &blockers_of(&[])),
            Severity::Failed
        );
        // 同一列再加上 dead／blocked：高的贏
        assert_eq!(
            worker_severity(&m, 2, &live_of(&["%1", "%2"]), &blockers_of(&[])),
            Severity::Dead,
            "死活蓋過 failed"
        );
        assert_eq!(
            worker_severity(
                &m,
                2,
                &live_of(&["%1", "%2"]),
                &blockers_of(&[("%3", Blocker::Prompt)])
            ),
            Severity::Blocked,
            "blocker 蓋過死活"
        );
        assert!(Severity::Blocked > Severity::Dead);
        assert!(Severity::Dead > Severity::Failed);
        assert!(Severity::Failed > Severity::None);
    }

    /// 組標頭徽章數的是**畫得出來的**成員：篩選中不得與括號裡的數字打架。
    #[test]
    fn group_badges_count_only_what_the_filter_left() {
        let m = triage_model();
        let live = live_of(&["%1", "%2"]);
        let bl = blockers_of(&[("%2", Blocker::Prompt)]);
        let all = group_visible_severities(&m, &Filter::default(), &live, &bl);
        let total: usize = all.values().map(|c| c.blocked + c.dead + c.failed).sum();
        assert_eq!(total, 2, "全體：一個 blocked、一個 dead");

        let f = Filter {
            query: "a1".into(),
        };
        let filtered = group_visible_severities(&m, &f, &live, &bl);
        let total: usize = filtered.values().map(|c| c.blocked + c.dead + c.failed).sum();
        assert_eq!(total, 0, "篩掉之後徽章 MUST 一起消失：{filtered:?}");
    }

    // ---- P5.4：blocker snippet ----

    /// snippet **只在升旗時留著**：去抖期間（畫面說沒有 blocker）不顯示、
    /// 降旗即清、整層 unknown 也清。
    #[test]
    fn a_snippet_only_survives_while_its_flag_is_up() {
        let lines = vec!["Do you want to proceed?".to_string(), "> 1. Yes".to_string()];
        let fresh = || HashMap::from([("%1".to_string(), lines.clone())]);

        let mut s = Snippets::default();
        // 升旗：留著
        s.apply(fresh(), &blockers_of(&[("%1", Blocker::Prompt)]));
        assert_eq!(s.get("%1").map(|v| v.len()), Some(2));

        // 去抖期間：這一輪 probe 命中，但顯示層還是 none → 不得先於判定顯示
        let mut s2 = Snippets::default();
        s2.apply(fresh(), &blockers_of(&[("%1", Blocker::None)]));
        assert_eq!(s2.get("%1"), None, "去抖未升旗 MUST NOT 顯示框內容");

        // 降旗即清：下一輪沒命中（沒有新 snippet），顯示層轉 none
        s.apply(HashMap::new(), &blockers_of(&[("%1", Blocker::None)]));
        assert_eq!(s.get("%1"), None, "降旗 MUST 清掉舊框");
        assert_eq!(s.len(), 0, "不得留下無主的快取");

        // 整層 unknown（tmux 停擺）：舊框同樣不得替「現在被擋住」背書
        let mut s3 = Snippets::default();
        s3.apply(fresh(), &blockers_of(&[("%1", Blocker::Prompt)]));
        s3.apply(HashMap::new(), &BlockerIndex::unknown());
        assert_eq!(s3.get("%1"), None, "unknown MUST 清掉舊框");
    }

    /// snippet 與 blocker 判定**同一輪、同一份畫面**產生（零新增 tmux 查詢的
    /// 前提）：`Prompt` 必有內容，其餘三態必無。
    #[test]
    fn the_probe_returns_the_snippet_from_the_same_capture() {
        let prompt = "Do you want to proceed?\n> 1. Yes\n  4. No\n\nesc to cancel";
        let cases = [
            (Some(false), Some(prompt), Blocker::Prompt, true),
            (Some(false), Some("idle output"), Blocker::None, false),
            (Some(true), Some(prompt), Blocker::Occluded, false),
            (Some(false), None, Blocker::Unknown, false),
        ];
        for (in_mode, screen, want, has_snip) in cases {
            let tmux = BlockerTmux { in_mode, screen };
            let (b, snip) = blocker_probe(&tmux, "%1");
            assert_eq!(b, want, "in_mode={in_mode:?} screen={screen:?}");
            assert_eq!(
                snip.is_some(),
                has_snip,
                "只有 Prompt 那一支有框內容：{b:?}"
            );
        }
        // 索引層同樣同源
        let tmux = BlockerTmux {
            in_mode: Some(false),
            screen: Some(prompt),
        };
        let (idx, snips) = BlockerIndex::query_with_snippets(&tmux, &["%1".to_string()]);
        assert_eq!(idx.get("%1"), Blocker::Prompt);
        assert!(snips.contains_key("%1"), "命中的 pane MUST 有框內容");
    }
}
