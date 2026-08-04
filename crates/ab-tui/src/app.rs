//! UI 狀態機（selection＋鍵位語意，tui-design.md §2／§3）。純狀態轉移，
//! 不碰 terminal 與副作用——副作用以 `Effect` 交回 run loop 執行。

use ab_core::registry::AgentSnapshot;
use ab_core::task::InFlight;

use crate::model::{Model, Row, RowKey, is_terminal_status, task_rows, worker_rows};

/// footer 保留的警告則數上限（畫面是有限的，但**丟掉**與**覆寫**是兩件事：
/// 這裡至少保證最近幾則留得住，且掉的那幾則有計數可見）。
pub const MAX_WARNINGS: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Panel {
    Workers,
    Tasks,
}

/// 當前聚焦面板的選中項（DETAIL 欄、`r`／`i`／`c` 的共同輸入）。
/// DETAIL 本身不可聚焦——它永遠顯示「當前聚焦面板的選中項」。
pub enum Sel<'m> {
    None,
    Worker(&'m AgentSnapshot),
    Task {
        task: &'m InFlight,
        /// 該 task 的收件 worker；registry 已無此 agent 時為 `None`
        worker: Option<&'m AgentSnapshot>,
    },
}

/// `Enter` 在當前選中列上的意義（Enter matrix 的四格，§9 P4.6）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EnterAct {
    /// 這一列上 Enter 不做事（沒有選中列）
    None,
    /// worker 列：focus 它的 pane
    Focus,
    /// task 列：讀全文
    Read,
}

/// **當前選中列真的按得動的鍵**（P4.6 切片 B 審查 minor）。
///
/// 為什麼要有這個型別：footer／`?` 的提示與 `dispatch_key` 的實際判定若各寫
/// 一份，兩者必然漂移——漂移的症狀是「畫面說按得動、按下去卻回一行拒絕訊息」，
/// 而那正是 contextual footer 要消滅的東西。這裡是**單一事實源**：提示由它
/// 組字，dispatch 由它決定放不放行；caps 說 false 的鍵產不出對應的 `Effect`。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RowCaps {
    pub enter: EnterAct,
    /// `r`：讀 task 全文（非終態的拒絕由 core 權威回答，不在這裡預判）
    pub read: bool,
    /// `i`：worker 摘要頁——task 列的收件人已不在 registry 時**沒有**摘要可看
    pub info: bool,
    pub copy: bool,
    /// `x`：終態任務沒有可取消的轉換
    pub cancel: bool,
    /// `e`：只對 worker 列有效。
    ///
    /// **刻意不看 `evict_inflight`**：那是同一列上的暫時狀態而不是「這一列有
    /// 沒有這個鍵」，而且按下去會得到「already in progress」這個有用的回覆
    /// ——把鍵從畫面上抽掉反而讓人以為自己按錯列。
    pub evict: bool,
    /// `L`：尾行預覽（P4.7 切片 D）。只對**有 pane 的 worker 列**有效——
    /// 沒有 pane 就沒有東西可 capture，task 列則沒有唯一的 pane 可言。
    ///
    /// 同 `evict`：**刻意不看 `peek_inflight`**，那是暫時狀態不是「這一列有沒有
    /// 這個鍵」，按下去會得到「already in progress」這個有用的回覆
    pub peek: bool,
}

impl RowCaps {
    const NONE: RowCaps = RowCaps {
        enter: EnterAct::None,
        read: false,
        info: false,
        copy: false,
        cancel: false,
        evict: false,
        peek: false,
    };
}

/// 當前選中列的能力（見 `RowCaps`）。
pub fn row_caps(app: &App, model: &Model) -> RowCaps {
    match app.selection(model) {
        Sel::None => RowCaps::NONE,
        Sel::Worker(w) => RowCaps {
            enter: EnterAct::Focus,
            info: true,
            copy: true,
            evict: true,
            // 沒有 pane 的列（registry 有紀錄、pane 欄空）capture 不到任何東西
            peek: !w.pane.is_empty(),
            ..RowCaps::NONE
        },
        Sel::Task { task, worker } => RowCaps {
            enter: EnterAct::Read,
            read: true,
            info: worker.is_some(),
            copy: true,
            cancel: !is_terminal_status(&task.status),
            evict: false,
            peek: false,
        },
    }
}

/// `r` 開的全螢幕 pager（§3：讀 task 全文）。**存原始 bytes**——action 層
/// 一律保留 bytes，只有 render 才 lossy 轉字串（gate (b) 比的就是 bytes）。
pub struct Pager {
    pub id: String,
    pub from: String,
    pub to: String,
    pub bytes: Vec<u8>,
    pub scroll: usize,
}

impl Pager {
    /// 捲動上界：最後一頁的頂端。內容比一頁短時是 0（沒得捲）。
    ///
    /// 列數問的是 `action::pager_lines`——render 畫的就是那一份，兩邊各算一次
    /// 就會漂移（F6）。
    pub fn max_scroll(&self, page: usize) -> usize {
        crate::action::pager_lines(self).len().saturating_sub(page)
    }
}

/// 與 crossterm 解耦的鍵表示：狀態機測試不需要 terminal 事件型別。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Char(char),
    /// filter 輸入模式的退格（P4.7 切片 C）。命令模式下沒有語意
    Backspace,
    Tab,
    /// `Shift+Tab`（crossterm 的 `KeyCode::BackTab`）：反向面板循環
    BackTab,
    Enter,
    Esc,
    Down,
    Up,
    /// 翻頁／到頂到底（P4.6 切片 C）。位移一律走與 `j`／`k` 同一條 selection
    /// 路徑（stable key），不是另一套索引運算
    PageUp,
    PageDown,
    Home,
    End,
}

/// 各面板的**可視高度**（列數）。PgUp／PgDn 的位移是「一頁」，而一頁多長只有
/// 版面知道——render 每幀量到之後回填到這裡（見 `view::panel_heights`），
/// 狀態機自己不碰 terminal 尺寸。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PageSizes {
    pub workers: u16,
    pub tasks: u16,
    /// `r` 的全螢幕 pager 可視高度（它不吃三欄版面，另量一份）
    pub pager: u16,
}

impl Default for PageSizes {
    /// 還沒畫過第一幀時的保守值（單元測試也用它）：小一點只是翻得慢，
    /// 大過真實高度才會翻過頭。
    fn default() -> Self {
        PageSizes {
            workers: 10,
            tasks: 10,
            pager: 10,
        }
    }
}

/// 待 run loop 執行的副作用。
#[derive(PartialEq, Eq, Debug)]
pub enum Effect {
    None,
    Quit,
    /// focus 該 pane（§2 focus 語意由 action 層落地）
    Focus {
        pane: String,
        label: String,
    },
    /// 確認後的 cancel（id immutable，天然 CAS，§5）
    Cancel {
        id: String,
    },
    /// `r`：讀 task 全文（走背景 worker——`acquire_lock` 會 block）
    Read {
        id: String,
    },
    /// `i`：worker 摘要頁（資料只來自 registry＋已載入的 read model，
    /// 不為它開新的 tmux 查詢）
    Info {
        worker: String,
    },
    /// `c`：把證據交給 clipboard（tmux buffer，走背景 worker）
    Copy {
        payload: String,
    },
    /// `e` 第一段：讀 registry 做出身判定＋取當下世代，開證據框
    /// （讀檔在 run loop，狀態機不碰 FS）
    EvictPrompt {
        worker: String,
    },
    /// `e` 第二段：證據框確認後執行。**只帶證據框上顯示過的值**——expect
    /// 參數要在確認當下重讀 registry 才算數（§5），不是沿用輪詢快照
    Evict {
        shown: crate::action::EvictShown,
    },
    /// `L`：尾行預覽（P4.7 切片 D）。one-shot，帶著**目標識別**出門——晚到的
    /// 結果要靠它比對，MUST NOT 貼到已經換過的 selection 上
    Peek {
        target: PeekTarget,
    },
}

/// 一次尾行預覽的目標（P4.7 切片 D）。
///
/// 為什麼不只帶 pane：pane id 會被 tmux 回收再發給別人，而 `RowKey` 帶著世代
/// （`(name, spawn_tag)`）。兩者一起比，晚到的結果才不會貼到「同一個 pane、
/// 不同一代」的列上——與 evict 的 compare-and-act 是同一條紀律：目標是**那一
/// 代**，不是那個名字。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PeekTarget {
    pub pane: String,
    pub name: String,
    pub key: RowKey,
}

/// 尾行預覽的 overlay 內容（純資料，render 只負責畫）。
///
/// 為什麼不塞進 `app.info`：`i` 那個 popup 溢出時**截尾**，而這裡要看的正是
/// 最後那幾行——用同一個容器等於把唯一有用的部分切掉。這裡改由 render 端
/// 底部對齊（見 `view::render_peek`）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PeekView {
    /// 框上的標題（worker 名＋pane，人要能確認自己看的是哪一列）
    pub title: String,
    pub lines: Vec<String>,
    /// byte 界截斷過：畫面 MUST 標記，否則人會以為那就是全部
    pub truncated: bool,
}

pub struct App {
    pub panel: Panel,
    /// 三個 `*_idx` 是 render 與 `j`／`k` 用的**位置快取**；跨 reload 的身分
    /// 是下面那三個 stable key（P4.6 切片 B）。兩個方向各只有一個入口：
    /// 位置→身分是 `sync_keys`（每個按鍵之後跑一次），身分→位置是 `relocate`
    /// （每輪磁碟重讀跑一次）。任何直接寫 `*_idx` 的路徑都會在同一次
    /// `handle_key` 內被 `sync_keys` 收斂回來。
    pub row_idx: usize,
    /// TASKS 欄的選中列（`task_rows` 的索引）
    pub task_idx: usize,
    /// WORKERS 的 stable key；欄空時為 `None`
    pub row_key: Option<RowKey>,
    /// TASKS 的 stable key＝immutable task id
    pub task_key: Option<String>,
    /// `x` 的單確認框：`Some(task-id)`＝等待 y/n。綁 id 不綁列位——確認期間
    /// 列表刷新也不會改變取消目標（§5 CAS）。
    pub confirm: Option<String>,
    pub help: bool,
    /// `r` 的全螢幕 overlay pager
    pub pager: Option<Pager>,
    /// `i` 的 worker 摘要頁（已組好的行；任意鍵關閉）
    pub info: Option<Vec<String>>,
    /// `e` 的證據框：綁按 e 當下讀到的 pane／世代，y 確認時據以比對
    pub evict_prompt: Option<crate::action::EvictPrompt>,
    /// 已在跑的 evict（worker 名）。同一個 worker 不得重複派收尾任務——
    /// 一次性 thread 各自獨立，沒有這道閘就會有兩條同時跑
    pub evict_inflight: std::collections::HashSet<String>,
    /// **警告不進 `message`**：footer 的單行 message 每則新訊息就被覆寫，
    /// `notify-failed`／「審計未落地」會被下一行進度或成功訊息蓋掉，人再也
    /// 看不到（codex 複核 major #2）。這裡是 sticky 的有界歷史：只 append、
    /// 終局訊息 MUST NOT 清掉它，由人按 `Esc` 確認後才清。
    pub warnings: Vec<String>,
    pub message: String,
    /// 各面板可視高度（PgUp／PgDn 的一頁），每幀由 run loop 回填
    pub pages: PageSizes,
    /// `/` 的畫面過濾（P4.7 切片 C）。純 UI 狀態：只決定哪幾列畫得出來，
    /// 不進 registry、不發查詢
    pub filter: crate::model::Filter,
    /// 正在輸入 filter：這個模式下**所有** `Key::Char` 都進緩衝區，不再當
    /// 命令鍵——否則打一個 `q` 就把 dashboard 關掉了
    pub filter_input: bool,
    /// TASKS 欄的 scope（`S` 切換）。預設 `All`＝現行語意
    pub scope: crate::model::Scope,
    /// `L` 的尾行預覽 overlay（任意鍵關閉，同 `i`）
    pub peek: Option<PeekView>,
    /// 正在跑的尾行預覽（P4.7 切片 D）。`Some`＝一次按鍵送出的那個 request
    /// 還沒回來，此時再按 `L` 不再送（同 `evict_inflight` 的閘）
    pub peek_inflight: Option<PeekTarget>,
}

impl App {
    pub fn new() -> Self {
        App {
            panel: Panel::Workers,
            row_idx: 0,
            task_idx: 0,
            row_key: None,
            task_key: None,
            confirm: None,
            help: false,
            pager: None,
            info: None,
            evict_prompt: None,
            evict_inflight: std::collections::HashSet::new(),
            warnings: Vec::new(),
            message: String::new(),
            pages: PageSizes::default(),
            filter: crate::model::Filter::default(),
            filter_input: false,
            scope: crate::model::Scope::default(),
            peek: None,
            peek_inflight: None,
        }
    }

    /// 追加一則 sticky 警告（有界，滿了丟最舊的）。
    ///
    /// 為什麼是「丟最舊」而不是「丟最新」：新的失敗通常是舊失敗的後果，最新
    /// 那則才是人此刻要處理的。連續重複的同一句只留一則（同一輪 evict 的
    /// notify-failed 不必洗版）。
    pub fn push_warning(&mut self, w: String) {
        if self.warnings.last() == Some(&w) {
            return;
        }
        self.warnings.push(w);
        while self.warnings.len() > MAX_WARNINGS {
            self.warnings.remove(0);
        }
    }

    /// 位置 → 身分。以索引改動 selection 的路徑（鍵盤）之後
    /// 都要走這一步，否則下一輪 reload 會拿舊身分把游標拉回去。
    ///
    /// **沒有選中列時保留舊 key**（不寫成 `None`）：`relocate` 會把「原本那一
    /// 列被同名新一代原地取代」表達成「不選任何列」，此刻若把 key 清掉，下一
    /// 個無關的按鍵（`?`、`Tab`…）就會讓下一輪 reload 重新自動選列——正好選回
    /// 那個新一代。舊 key 留著，這個狀態才停得住。
    pub fn sync_keys(&mut self, model: &Model) {
        if let Some(r) = self.selected_row(model) {
            self.row_key = Some(crate::model::row_key(model, r));
        }
        if let Some(&ti) = self.task_rows(model).get(self.task_idx) {
            self.task_key = Some(model.recent[ti].id.clone());
        }
    }

    /// 身分 → 位置（每輪 500ms 磁碟重讀走這一條）。找得到就回到**原本那一
    /// 項**，不管它現在排第幾——人沒動選取時畫面就不會跳列。找不到（該項真的
    /// 沒了）才落鄰列。
    pub fn relocate(&mut self, model: &Model) {
        let rows = self.rows(model);
        let old_row = self.row_idx;
        match self
            .row_key
            .as_ref()
            .and_then(|k| {
                rows.iter()
                    .position(|r| crate::model::row_key(model, *r) == *k)
            })
            .or_else(|| fallback_row(model, &rows, old_row, self.row_key.as_ref()))
        {
            Some(i) => self.row_idx = i,
            // 一列都不能落＝原本那一列被**同名新一代**原地取代，而且旁邊沒有
            // 別的列可退。此時明確「不選任何列」（索引落在列數之外）：DETAIL
            // 空著、`e`／`x` 一律拒絕，人按 j／k 才重新選。
            // 靜默接續到新一代才是真正危險的——`e` 的目標會在人沒重選的情況下
            // 換成另一個世代（§5 CAS 比對的正是世代）
            None => {
                // 只在「原本真的選著一列」的那一輪報一次：之後 row_idx 已在
                // 範圍外，不會每 500ms 把別的訊息洗掉
                let dropped = match self.row_key.as_ref() {
                    Some(RowKey::Worker { name, .. }) if old_row < rows.len() => Some(name.clone()),
                    _ => None,
                };
                if let Some(name) = dropped {
                    self.message =
                        format!("'{name}' was replaced by a new generation; press j/k to select");
                }
                self.row_idx = rows.len();
            }
        }

        let trows = self.task_rows(model);
        let ti = self
            .task_key
            .as_ref()
            .and_then(|k| trows.iter().position(|&i| model.recent[i].id == *k))
            .unwrap_or_else(|| neighbor(self.task_idx, trows.len()));
        self.task_idx = ti;

        // 落到鄰列的那些軸，身分要跟著改寫成新落點——否則舊 key 每一輪都會
        // 再試著定位一次，人往下移一列又被拉回來
        self.sync_keys(model);
    }

    pub fn rows(&self, model: &Model) -> Vec<Row> {
        worker_rows(model, &self.filter)
    }

    pub fn selected_row(&self, model: &Model) -> Option<Row> {
        self.rows(model).get(self.row_idx).copied()
    }

    /// TASKS 欄的列（`model.recent` 的索引，含終態）。
    pub fn task_rows(&self, model: &Model) -> Vec<usize> {
        task_rows(model, &self.filter, self.scope)
    }

    /// 當前聚焦面板的選中項（DETAIL 與 `r`／`i`／`c` 共用的單一入口，
    /// 免得每個鍵各寫一份索引運算）。
    pub fn selection<'m>(&self, model: &'m Model) -> Sel<'m> {
        match self.panel {
            Panel::Workers => match self.selected_row(model) {
                Some(Row::Worker(wi)) => Sel::Worker(&model.workers[wi]),
                Some(Row::Task { worker, task }) => Sel::Task {
                    task: &model.tasks[task],
                    worker: Some(&model.workers[worker]),
                },
                None => Sel::None,
            },
            // TASKS 欄的列**沒有** `Row` 那種確切綁定（它是全 pool 的平坦
            // 列表），所以這裡要現場判「這筆 task 說得出唯一的 worker 嗎」。
            // 走 `worker_of_task`（＝`attached()`）而不是純名字比對：後者會讓
            // Unattached 的歷史 task 一被選中就認領當代的同名 worker，DETAIL
            // 於是顯示當代 pane／blocker、`i` 按得動、`c` 帶當代 pane
            // （修正輪 R2／F3）
            Panel::Tasks => match self.task_rows(model).get(self.task_idx) {
                Some(&ti) => {
                    let task = &model.recent[ti];
                    Sel::Task {
                        task,
                        worker: crate::model::worker_of_task(model, task)
                            .map(|wi| &model.workers[wi]),
                    }
                }
                None => Sel::None,
            },
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// 這一列是不是「**同名、不同世代**」——也就是原本選中的那個 worker 被
/// respawn 之後原地取代的那一列（P4.6 切片 B 審查 major）。
///
/// 它與原本那一列同名、常常還在同一個索引上，所以純索引的鄰列回退會**無聲**
/// 落在它身上；而世代正是 evict CAS 的比對軸，選取悄悄換代等於把破壞性動作
/// 指向另一個對象。這種列一律跳過。
fn is_replacement(model: &Model, row: Row, old: Option<&RowKey>) -> bool {
    match (row, old) {
        (
            Row::Worker(wi),
            Some(RowKey::Worker {
                name,
                spawn_tag: old_tag,
            }),
        ) => model.workers[wi].name == *name && model.workers[wi].spawn_tag != *old_tag,
        _ => false,
    }
}

/// WORKERS 欄選中項消失後的落點：**前一列 → 原位（刪除時等於後一列）→ 後一
/// 列 → 第 0 列**，且一律跳過 `is_replacement` 的那一列。
///
/// 為什麼「原位」與「後一列」都要試：兩種消失方式的索引語意不同——列被刪掉
/// 時原索引指的就是原本的後一列；被同名新一代原地取代時，真正的後一列在
/// 原索引＋1。
///
/// 回傳 `None`＝一列都不能落（見 `relocate` 的處置）。
fn fallback_row(model: &Model, rows: &[Row], old: usize, key: Option<&RowKey>) -> Option<usize> {
    [old.checked_sub(1), Some(old), Some(old + 1), Some(0)]
        .into_iter()
        .flatten()
        .find(|&i| i < rows.len() && !is_replacement(model, rows[i], key))
}

/// 選中項消失後的落點（P4.6 切片 B）：**前一列 → 後一列 → 第 0 列**。
///
/// 「前一列」優先是因為列表多半是「上面是舊的、下面是新的」；選中項被刪掉時
/// 往上收斂比往下跳穩定（往下那一列在下一輪可能也不見了）。`old` 是它消失前
/// 的位置——刪掉一列之後，`old` 這個索引本身指的就是原本的**後一列**。
fn neighbor(old: usize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if old > 0 && old - 1 < len {
        old - 1
    } else if old < len {
        old
    } else {
        0
    }
}

/// 鍵位表（§3 中屬第一縱切的子集）。回傳待執行的副作用。
///
/// 收尾一律走 `sync_keys`：任何分支改了 `*_idx` 之後，stable key 都在同一次
/// 呼叫內被寫回來（分支各自記得同步＝遲早有一條會忘記，而忘記的症狀是
/// 「reload 之後游標自己跑掉」這種難查的間歇現象）。
pub fn handle_key(app: &mut App, model: &Model, key: Key) -> Effect {
    let eff = dispatch_key(app, model, key);
    app.sync_keys(model);
    eff
}

fn dispatch_key(app: &mut App, model: &Model, key: Key) -> Effect {
    // 確認框開著：只認 y／Enter（執行）與 n／Esc（放棄），其餘鍵一律吞掉
    // ——破壞性動作的模態下不得讓導航鍵改變 selection 造成誤解
    if let Some(id) = app.confirm.clone() {
        return match key {
            Key::Char('y') | Key::Enter => {
                app.confirm = None;
                Effect::Cancel { id }
            }
            Key::Char('n') | Key::Esc => {
                app.confirm = None;
                app.message = "cancel aborted".to_string();
                Effect::None
            }
            _ => Effect::None,
        };
    }
    // `e` 的證據框開著：與 `x` 同樣的模態紀律（只認 y／Enter 與 n／Esc），
    // 其餘鍵一律吞掉。確認帶的是**框上顯示過的那組值**，執行前還要重讀比對
    if let Some(p) = app.evict_prompt.as_ref() {
        let shown = p.shown.clone();
        return match key {
            Key::Char('y') | Key::Enter => {
                app.evict_prompt = None;
                Effect::Evict { shown }
            }
            Key::Char('n') | Key::Esc => {
                app.evict_prompt = None;
                app.message = "evict aborted".to_string();
                Effect::None
            }
            _ => Effect::None,
        };
    }
    // `r` 的 pager 開著：只認捲動與關閉。導航鍵在這裡被吞掉——overlay 期間
    // 底層 selection MUST 不動（關掉之後人才回得到原本那一列）。
    //
    // **翻頁鍵在這裡是捲動，不是吞掉**（審查 F6）：PgUp／PgDn／Home／End 與
    // j／k 是同一族（捲動語意），而 footer 與 `?` 頁都列著它們——畫面上列出
    // 的鍵按下去 MUST 真的有那個效果。cancel／evict 確認框吞鍵是另一回事：
    // 破壞性確認框本來就該吞掉一切非確認鍵。
    if let Some(p) = app.pager.as_mut() {
        // 下界一律夾在最後一頁：捲過尾端只會得到整片空白，那不是「到底」
        let page = app.pages.pager.max(1) as usize;
        let max = p.max_scroll(page);
        match key {
            Key::Char('j') | Key::Down => p.scroll = (p.scroll + 1).min(max),
            Key::Char('k') | Key::Up => p.scroll = p.scroll.saturating_sub(1),
            Key::PageDown => p.scroll = p.scroll.saturating_add(page).min(max),
            Key::PageUp => p.scroll = p.scroll.saturating_sub(page),
            Key::Home => p.scroll = 0,
            Key::End => p.scroll = max,
            Key::Esc | Key::Char('q') => app.pager = None,
            _ => {}
        }
        return Effect::None;
    }
    if app.info.is_some() {
        app.info = None; // 任意鍵關閉（沿用 help overlay 的慣例）
        return Effect::None;
    }
    // `L` 的尾行預覽：同樣任意鍵關閉。**不因此取消 in-flight 的請求**——閘是
    // 由回信放開的，關掉畫面不代表子行程回來了
    if app.peek.is_some() {
        app.peek = None;
        return Effect::None;
    }
    // `/` 的輸入模式（P4.7 切片 C）。**攔截順序刻意排在 overlay 之後**：
    // 確認框／pager／摘要頁是模態的，它們在的時候連 `/` 都不該進來。
    //
    // 進來之後所有 `Key::Char` 都是內容（含 `/` 自己）——把 `q`／`x` 留成命令
    // 鍵的話，打字打到一半就會把 dashboard 關掉或開出一個 cancel 確認框。
    if app.filter_input {
        match key {
            Key::Char(c) => app.filter.query.push(c),
            // Backspace 目前不在 `Key` 列舉裡（crossterm 端也還沒對映），
            // 退格走的是這一條：見 `Key::Backspace`
            Key::Backspace => {
                app.filter.query.pop();
            }
            // Enter＝收工但**保留** filter（人要的是「篩完之後開始操作」）
            Key::Enter => app.filter_input = false,
            // Esc＝取消：清空並離開（filter 是唯一被清掉的東西，警告不動）
            Key::Esc => {
                app.filter.query.clear();
                app.filter_input = false;
            }
            _ => {}
        }
        // 列數變了：把選取夾回範圍內（走既有的 relocate／stable key 路徑）
        app.relocate(model);
        return Effect::None;
    }
    if app.help {
        app.help = false; // 任意鍵關閉
        return Effect::None;
    }
    match key {
        Key::Char('q') => Effect::Quit,
        // Esc（無 overlay 時）＝我看過了：清掉 sticky 警告。警告 MUST 只由
        // 人的顯式動作清除，不得被下一則訊息覆寫掉（major #2）
        Key::Esc => {
            if app.warnings.is_empty() {
                Effect::None
            } else {
                let n = app.warnings.len();
                app.warnings.clear();
                app.message = format!("cleared {n} warning(s)");
                Effect::None
            }
        }
        Key::Char('?') => {
            app.help = true;
            Effect::None
        }
        // `/`：進 filter 輸入模式（P4.7 切片 C）。**不清掉既有查詢**——再按一次
        // `/` 是「繼續編輯剛才那條」，要清空按 Esc
        Key::Char('/') => {
            app.filter_input = true;
            Effect::None
        }
        // `S`：切 TASKS 欄的 scope（all ⇄ unattached）。純顯示面，不動任何資料。
        //
        // **為什麼是大寫**：§3 鍵位表把小寫 `s` 保留給 send（建立性動作，尚未
        // 實作但從未撤回）。拿它當 scope 切換等於「文件說按 s 會 send、按下去
        // 卻翻 scope」——那正是 contextual footer 要消滅的漂移
        Key::Char('S') => {
            app.scope = app.scope.toggled();
            app.message = format!("tasks scope: {}", app.scope.label());
            app.relocate(model);
            Effect::None
        }
        // 兩欄循環（DETAIL 不可聚焦：它只是選中項的投影）。
        // **ORIGINS 退場後只剩兩欄**（P4.7 切片 B）：Tab 與 Shift+Tab 於是
        // 是同一個來回。兩個鍵都留著——手指記得的是「Tab 換欄」，把反向鍵
        // 抽掉只會讓人按了沒反應
        Key::Tab => {
            app.panel = match app.panel {
                Panel::Workers => Panel::Tasks,
                Panel::Tasks => Panel::Workers,
            };
            Effect::None
        }
        // 反向循環（`Shift+Tab`）。**P4 效率量測驅動的 additive 補入**：
        // 首輪量到 TUI 4 步／baseline 7 步（57%，未達 §9 P4 的 ≤50%），而那
        // 4 步裡有 2 步純粹是「從初始的 WORKERS 單向繞到 OWNERS」的固定開銷。
        // 補上反向鍵之後同一份 replay script 是 3 步（43%）。純鍵位面 additive，
        // 不動 selection 起點、不動任何協定語意。
        Key::BackTab => {
            app.panel = match app.panel {
                Panel::Workers => Panel::Tasks,
                Panel::Tasks => Panel::Workers,
            };
            Effect::None
        }
        Key::Char('j') | Key::Down => {
            move_sel(app, model, Move::By(1));
            Effect::None
        }
        Key::Char('k') | Key::Up => {
            move_sel(app, model, Move::By(-1));
            Effect::None
        }
        // 翻頁／到頂到底（P4.6 切片 C）。作用在**當前焦點面板**，與 j／k 同一
        // 條路徑：一律經 `move_sel`（夾在範圍內、換 origin 時重置、之後由
        // `handle_key` 收尾的 `sync_keys` 寫回 stable key），不另開索引運算。
        // 空面板時 `move_sel` 直接返回——不動、不 panic、不報錯
        Key::PageDown => {
            move_sel(app, model, page_move(app, true));
            Effect::None
        }
        Key::PageUp => {
            move_sel(app, model, page_move(app, false));
            Effect::None
        }
        Key::Home => {
            move_sel(app, model, Move::First);
            Effect::None
        }
        Key::End => {
            move_sel(app, model, Move::Last);
            Effect::None
        }
        // Enter matrix（P4.6 切片 B）：Enter＝「打開我選的這一項」，各欄的
        // 「打開」是什麼由 `row_caps` 這個**單一事實源**決定——footer 的提示
        // 讀的是同一份 caps，於是提示與實際行為不可能各說各話
        Key::Enter => match row_caps(app, model).enter {
            // worker 列：打開＝focus 它的 pane（語意逐字不變，含 popup 協定）
            EnterAct::Focus => match app.selected_row(model) {
                Some(Row::Worker(wi)) => {
                    let w = &model.workers[wi];
                    Effect::Focus {
                        pane: w.pane.clone(),
                        label: w.name.clone(),
                    }
                }
                _ => Effect::None,
            },
            // task 列：打開＝讀它的全文（`r` 的 alias，同一個 id、同一條
            // 路徑）——**不再代人跳去它的 worker**：人在 task 列上選的是任務，
            // 想看的是內容；worker 就在上一列，要 focus 按 k 再 Enter 即可
            EnterAct::Read => read_effect(app, model),
            EnterAct::None => Effect::None,
        },
        // `x` 的合法目標只有 task 列（§2 selection model：否則沒有唯一的
        // immutable id 可綁），且 TASKS 欄還多一道終態閘——終態任務 cancel
        // 不了，開確認框只會讓人以為做得到
        Key::Char('x') => {
            let cancel = row_caps(app, model).cancel;
            match app.selection(model) {
                Sel::Task { task, .. } if cancel => {
                    app.confirm = Some(task.id.clone());
                    Effect::None
                }
                Sel::Task { task, .. } => {
                    app.message = format!(
                        "task {} is already terminal ({}); nothing to cancel",
                        task.id, task.status
                    );
                    Effect::None
                }
                _ => {
                    app.message =
                        "x only acts on task rows (cancel needs a unique task id)".to_string();
                    Effect::None
                }
            }
        }
        // `r`：讀 task 全文（等價 `agent-bridge read <id>`，走同一份 core
        // 實作）。合法目標＝任何帶 task id 的選中列。**與 task 列的 Enter 是
        // 同一個函式**——alias 的意思是同一條路徑，不是兩份長得像的程式碼
        Key::Char('r') => read_effect(app, model),
        // `i`：worker 摘要頁。task 列取其所屬 worker——收件人已不在 registry
        // 時沒有摘要可看，caps 那邊也是 `info: false`（footer 不會提示它）
        Key::Char('i') => {
            let info = row_caps(app, model).info;
            match app.selection(model) {
                Sel::Worker(w) if info => Effect::Info {
                    worker: w.name.clone(),
                },
                Sel::Task {
                    worker: Some(w), ..
                } if info => Effect::Info {
                    worker: w.name.clone(),
                },
                Sel::Task { task, worker: None } => {
                    app.message =
                        format!("'{}' is gone from the registry; no info to show", task.to);
                    Effect::None
                }
                _ => {
                    app.message = "i only acts on worker / task rows".to_string();
                    Effect::None
                }
            }
        }
        // `e`：evict 選中 worker（§3／§5）。合法目標**只有 worker 列**——
        // 破壞性動作要有唯一且明確的目標，task 列上按 e 不代人推論它的 worker。
        // 出身判定與當下世代的讀取都在 run loop（要碰 FS），這裡只發動
        Key::Char('e') => match app.selection(model) {
            Sel::Worker(w) if row_caps(app, model).evict => {
                if app.evict_inflight.contains(&w.name) {
                    app.message = format!("evict of '{}' is already in progress", w.name);
                    Effect::None
                } else {
                    Effect::EvictPrompt {
                        worker: w.name.clone(),
                    }
                }
            }
            _ => {
                app.message =
                    "e only acts on worker rows (evict targets a worker, not a task)".to_string();
                Effect::None
            }
        },
        // `L`：尾行預覽（P4.7 切片 D）。**大寫**是契約：§3 的鍵位表上大小寫
        // 各有指派（`s` send／`S` scope），順手改成小寫會讓下一次擴充撞上。
        //
        // 這裡只發動：三重有界的取得路徑在 core（`tmux::capture_pane_tail`），
        // 一次性 thread 在 run loop——UI thread 不碰 tmux（§4 硬條款）
        Key::Char('L') => {
            if !row_caps(app, model).peek {
                app.message =
                    "L only acts on worker rows that have a pane (nothing to capture)".to_string();
                return Effect::None;
            }
            // **一次按鍵一個 request**：沒回來之前再按不再送。這一則訊息是有用
            // 的回覆（人知道自己按到了），不是拒絕
            if let Some(t) = app.peek_inflight.as_ref() {
                app.message = format!("preview of '{}' is already in progress", t.name);
                return Effect::None;
            }
            match peek_target(app, model) {
                Some(target) => Effect::Peek { target },
                // caps 已經放行，這裡拿不到目標只可能是列在這一瞬消失
                None => Effect::None,
            }
        }
        // `c`：複製**證據**（唯讀命令原文＋immutable id），MUST NOT 複製任何
        // mutation 命令（§5 顯示紀律）
        Key::Char('c') => {
            let payload = if row_caps(app, model).copy {
                crate::action::copy_payload(&app.selection(model))
            } else {
                String::new()
            };
            if payload.is_empty() {
                app.message = "c only acts on worker / task rows (nothing is selected)".to_string();
                Effect::None
            } else {
                Effect::Copy { payload }
            }
        }
        _ => Effect::None,
    }
}

/// 當前選中列的預覽目標（`None`＝這一列預覽不了）。
///
/// **與 `row_caps().peek` 同一個條件**：worker 列且 pane 非空。兩處各寫一份
/// 判斷就會出現「footer 說按得動、按下去沒反應」——那正是 contextual footer
/// 要消滅的東西。
pub fn peek_target(app: &App, model: &Model) -> Option<PeekTarget> {
    let row = app.selected_row(model)?;
    let Row::Worker(wi) = row else {
        return None;
    };
    let w = &model.workers[wi];
    if w.pane.is_empty() {
        return None;
    }
    Some(PeekTarget {
        pane: w.pane.clone(),
        name: w.name.clone(),
        key: crate::model::row_key(model, row),
    })
}

/// 預覽結果回到 UI（P4.7 切片 D）。**純函式、不碰 terminal**：run loop 只把
/// 一次性 thread 的回信轉進來，判斷全在這裡，gate (d) 的「晚到結果不貼新
/// selection」才驗得到。
///
/// 兩件事一定會發生：**閘一律放開**（否則失敗一次就再也按不動），以及**目標
/// 比對**——回來時當前選中列若已不是當初那一個（換列、換代、被 filter 篩掉），
/// 結果丟棄。丟棄要說出來，不然人會以為 `L` 沒作用。
pub fn peek_apply(
    app: &mut App,
    model: &Model,
    target: &PeekTarget,
    res: Option<ab_core::tmux::TailCapture>,
) {
    app.peek_inflight = None;
    if peek_target(app, model).as_ref() != Some(target) {
        app.message = format!(
            "preview of '{}' discarded (the selection moved on while it was running)",
            target.name
        );
        return;
    }
    match res {
        Some(cap) => {
            app.peek = Some(crate::action::peek_page(target, &cap));
            app.message.clear();
        }
        // 逾時／tmux 起不來／pane 已不在：**只影響這一次預覽**，畫面照跑
        None => {
            app.message = format!(
                "preview of '{}' ({}) timed out or is unavailable",
                target.name, target.pane
            );
        }
    }
}

/// read 的單一入口（`r` 與 task 列的 `Enter` 共用）：非終態的拒絕由 core 那一
/// 側權威回答，這裡不預判、不造旁路。
fn read_effect(app: &mut App, model: &Model) -> Effect {
    let read = row_caps(app, model).read;
    match app.selection(model) {
        Sel::Task { task, .. } if read => Effect::Read {
            id: task.id.clone(),
        },
        _ => {
            app.message = "r only acts on task rows (read needs a unique task id)".to_string();
            Effect::None
        }
    }
}

/// selection 的位移方式（`j`／`k`／PgUp／PgDn／Home／End 共用一條路徑）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Move {
    By(i64),
    /// WORKERS 欄的翻頁：位移量是**一個 viewport 的 rendered lines**，不是
    /// 「幾列」——組標頭佔行不佔列，把行高直接當列數會永久跳過某些列
    /// （切片 B1 修正輪 G2）。TASKS 欄沒有標頭，行＝列，仍走 `By`
    Page {
        lines: usize,
        down: bool,
    },
    First,
    Last,
}

/// 當前焦點面板的一頁是幾列（PgUp／PgDn 的位移量）。
///
/// 至少 1：畫面被壓到連一列都放不下時，翻頁仍要能動一列，不然那兩個鍵在窄
/// 畫面上會變成啞鍵。
fn page_len(app: &App) -> i64 {
    let h = match app.panel {
        Panel::Workers => app.pages.workers,
        Panel::Tasks => app.pages.tasks,
    };
    i64::from(h).max(1)
}

/// 翻頁的位移方式：WORKERS 走行號換算（組標頭），TASKS 行＝列直接位移。
fn page_move(app: &App, down: bool) -> Move {
    match app.panel {
        Panel::Workers => Move::Page {
            lines: page_len(app) as usize,
            down,
        },
        Panel::Tasks => Move::By(if down { page_len(app) } else { -page_len(app) }),
    }
}

fn move_sel(app: &mut App, model: &Model, mv: Move) {
    // 翻頁要換算行號，而行號吃的是**過濾後**的列（借出 `app` 之前先複製一份）
    let filter = app.filter.clone();
    let (idx, len) = match app.panel {
        Panel::Workers => {
            let n = app.rows(model).len();
            (&mut app.row_idx, n)
        }
        Panel::Tasks => {
            let n = app.task_rows(model).len();
            (&mut app.task_idx, n)
        }
    };
    if len == 0 {
        return;
    }
    // 一律夾在範圍內＝翻頁到邊界飽和，不 wrap（wrap 會讓「一直按 PgDn」在
    // 末尾突然跳回頂端，那是誤導而不是效率）
    let cur = match mv {
        Move::By(d) => *idx as i64 + d,
        Move::Page { lines, down } => {
            crate::model::worker_page_row(model, &filter, *idx, lines, down) as i64
        }
        Move::First => 0,
        Move::Last => len as i64 - 1,
    };
    *idx = cur.clamp(0, len as i64 - 1) as usize;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fixture 的 `groups` 一律由 `group_by_lineage` 推導（與 `Model::load`
    /// 同一條路徑）——fixture 自己寫一份分組等於驗自己抄的答案。
    fn with_groups(mut m: Model) -> Model {
        m.groups = crate::model::group_by_lineage(&m.workers);
        m
    }

    fn model() -> Model {
        with_groups(Model {
            groups: Vec::new(),
            workers: vec![
                AgentSnapshot {
                    name: "w1".into(),
                    pane: "%5".into(),
                    runtime: "codex".into(),
                    owner: "it:@1".into(),
                    ready: "ready".into(),
                    spawn_tag: "t-gen1".into(),
                    registered_at: "2026-07-31T00:00:00Z".into(),
                    spawned: true,
                    corrupt: false,
                    // P4.7 切片 A：lineage 兩欄對這些 fixture 無關（None＝欄位缺席）
                    lineage_root: None,
                    parent_agent: None,
                },
                AgentSnapshot {
                    name: "w2".into(),
                    pane: "%6".into(),
                    runtime: "agy".into(),
                    owner: "it:@1".into(),
                    ready: "ready".into(),
                    spawn_tag: "t-gen1".into(),
                    registered_at: "2026-07-31T00:00:00Z".into(),
                    spawned: true,
                    corrupt: false,
                    // P4.7 切片 A：lineage 兩欄對這些 fixture 無關（None＝欄位缺席）
                    lineage_root: None,
                    parent_agent: None,
                },
            ],
            tasks: vec![task("20260731T000001Z-aaaa", "w1", "queued")],
            // TASKS 欄的資料源：反序、含終態（第 0 列是完成的那筆）
            recent: vec![
                task("20260731T000009Z-dddd", "w1", "completed"),
                task("20260731T000001Z-aaaa", "w1", "queued"),
            ],
            recent_truncated: false,
        })
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

    fn task(id: &str, to: &str, status: &str) -> InFlight {
        InFlight {
            created_at: iso_from_id(id),
            id: id.into(),
            from: "alice".into(),
            to: to.into(),
            status: status.into(),
        }
    }

    // ── P4.7 切片 C：filter 輸入模式與 scope ─────────────────────────

    /// `/` 進輸入模式之後，**所有** `Key::Char` 都是內容。
    ///
    /// 這條鎖的是最容易踩的那個坑：把 `q`／`x` 留成命令鍵的話，打字打到一半
    /// 就會關掉 dashboard 或開出一個 cancel 確認框。
    #[test]
    fn filter_input_swallows_command_keys() {
        let m = model();
        let mut app = App::new();
        assert_eq!(handle_key(&mut app, &m, Key::Char('/')), Effect::None);
        assert!(app.filter_input, "`/` MUST 進輸入模式");
        for c in ['q', 'x', 'w', '1', '/'] {
            assert_eq!(
                handle_key(&mut app, &m, Key::Char(c)),
                Effect::None,
                "輸入模式中 '{c}' MUST NOT 觸發任何副作用"
            );
        }
        assert_eq!(app.filter.query, "qxw1/", "含 `/` 本身都是內容");
        assert!(app.confirm.is_none(), "`x` MUST NOT 開確認框");
        // Backspace 退一格
        handle_key(&mut app, &m, Key::Backspace);
        assert_eq!(app.filter.query, "qxw1");
        // Enter：離開輸入模式但**保留** filter
        handle_key(&mut app, &m, Key::Enter);
        assert!(!app.filter_input);
        assert_eq!(app.filter.query, "qxw1");
        // 離開之後 `q` 又是命令鍵
        assert_eq!(handle_key(&mut app, &m, Key::Char('q')), Effect::Quit);
    }

    /// Esc 在輸入模式＝清空並離開；再按 `/` 是接著編輯（不自動清空）。
    #[test]
    fn esc_clears_the_filter_and_slash_resumes_editing() {
        let m = model();
        let mut app = App::new();
        handle_key(&mut app, &m, Key::Char('/'));
        handle_key(&mut app, &m, Key::Char('w'));
        handle_key(&mut app, &m, Key::Esc);
        assert!(!app.filter_input);
        assert_eq!(app.filter.query, "", "Esc MUST 清空");

        handle_key(&mut app, &m, Key::Char('/'));
        handle_key(&mut app, &m, Key::Char('w'));
        handle_key(&mut app, &m, Key::Enter);
        handle_key(&mut app, &m, Key::Char('/'));
        assert_eq!(app.filter.query, "w", "再按 `/` MUST NOT 清掉既有查詢");
    }

    /// filter 篩到一列都不剩：DETAIL 落 `Sel::None`，導航鍵不 panic。
    #[test]
    fn an_empty_filter_result_selects_nothing_and_never_panics() {
        let m = model();
        let mut app = App::new();
        handle_key(&mut app, &m, Key::Char('/'));
        for c in "zzz".chars() {
            handle_key(&mut app, &m, Key::Char(c));
        }
        handle_key(&mut app, &m, Key::Enter);
        assert!(app.rows(&m).is_empty());
        assert!(app.task_rows(&m).is_empty());
        assert!(matches!(app.selection(&m), Sel::None));
        // 導航／動作鍵一律安全（空面板時 `move_sel` 直接返回）
        for k in [
            Key::Char('j'),
            Key::Char('k'),
            Key::Home,
            Key::End,
            Key::PageDown,
            Key::PageUp,
            Key::Char('x'),
            Key::Char('e'),
            Key::Char('r'),
            Key::Enter,
        ] {
            handle_key(&mut app, &m, k);
        }
        assert!(matches!(app.selection(&m), Sel::None));
    }

    /// filter 生效時 selection 走的仍是既有的 stable key／`relocate` 路徑：
    /// 篩掉選中列之後不得指到別人身上。
    #[test]
    fn filtering_reanchors_the_selection_through_the_stable_key() {
        let m = model();
        let mut app = App::new();
        // 先選到 w2（列序：w1、w1 的 task、w2）
        handle_key(&mut app, &m, Key::Char('j'));
        handle_key(&mut app, &m, Key::Char('j'));
        assert!(matches!(app.selection(&m), Sel::Worker(w) if w.name == "w2"));
        // 篩掉 w2：選取必須落在還在的列上，而不是留著舊索引指向別人
        handle_key(&mut app, &m, Key::Char('/'));
        for c in "w1".chars() {
            handle_key(&mut app, &m, Key::Char(c));
        }
        handle_key(&mut app, &m, Key::Enter);
        assert!(app.rows(&m).iter().all(|r| !matches!(
            r,
            Row::Worker(wi) if m.workers[*wi].name == "w2"
        )));
        match app.selection(&m) {
            Sel::Worker(w) => assert_eq!(w.name, "w1"),
            Sel::Task { task, .. } => assert_eq!(task.to, "w1"),
            Sel::None => panic!("還有列在，不該落 None"),
        }
    }

    /// **無主 task 的 caps**（修正輪 R2／F3）：TASKS 欄選中一筆掛不上任何
    /// worker 的 task 時，`Sel::Task.worker` MUST 是 `None`，`i` 也 MUST NOT
    /// 可按——`row_caps` 是 footer 提示與 `dispatch_key` 的同一份正本，
    /// 這裡一鬆，畫面就會提示一個按下去只會誤導人的鍵。
    #[test]
    fn an_unattached_task_has_no_worker_and_no_info() {
        let mut m = model();
        // w1 於 12:00 才註冊；這筆 task 是 11:00 建立的（前一代的）
        m.workers[0].registered_at = "2026-07-31T12:00:00Z".into();
        m.recent = vec![task("20260731T110000Z-aaaa", "w1", "running")];
        m.tasks = vec![task("20260731T110000Z-aaaa", "w1", "running")];
        let mut app = App::new();
        app.panel = Panel::Tasks;
        app.task_idx = 0;
        match app.selection(&m) {
            Sel::Task { task, worker } => {
                assert_eq!(task.id, "20260731T110000Z-aaaa");
                assert!(worker.is_none(), "MUST NOT 認領當代同名 worker");
            }
            _ => panic!("應該選中一筆 task"),
        }
        assert!(!row_caps(&app, &m).info, "`i` MUST NOT 可按");
        // 按下去也只有訊息，不發任何 Effect
        assert_eq!(handle_key(&mut app, &m, Key::Char('i')), Effect::None);

        // 對照組：同一筆 task 換成 13:00 建立（晚於註冊）→ 掛得上，`i` 可按
        m.recent = vec![task("20260731T130000Z-bbbb", "w1", "running")];
        m.tasks = vec![task("20260731T130000Z-bbbb", "w1", "running")];
        let mut app2 = App::new();
        app2.panel = Panel::Tasks;
        assert!(row_caps(&app2, &m).info, "掛得上時 `i` MUST 可按（對照組）");
    }

    /// `S` 切 TASKS scope（純顯示面：不動任何資料，也不影響 WORKERS 欄）。
    /// 小寫 `s` MUST NOT 有作用——§3 把它保留給 send。
    #[test]
    fn shift_s_toggles_the_tasks_scope() {
        let m = model();
        let mut app = App::new();
        assert_eq!(app.scope, crate::model::Scope::All);
        let workers_before = app.rows(&m);
        assert_eq!(handle_key(&mut app, &m, Key::Char('s')), Effect::None);
        assert_eq!(
            app.scope,
            crate::model::Scope::All,
            "小寫 s 是 §3 保留給 send 的鍵，不得成為 scope 切換"
        );
        assert_eq!(handle_key(&mut app, &m, Key::Char('S')), Effect::None);
        assert_eq!(app.scope, crate::model::Scope::Unattached);
        assert!(app.message.contains("unattached"), "實際：{}", app.message);
        // fixture 的兩筆 recent 都掛得上 w1（時戳晚於 registered_at）→ 空
        assert!(app.task_rows(&m).is_empty(), "Unattached 應為空");
        assert_eq!(app.rows(&m), workers_before, "WORKERS 欄不受 scope 影響");
        handle_key(&mut app, &m, Key::Char('S'));
        assert_eq!(app.scope, crate::model::Scope::All);
        assert_eq!(app.task_rows(&m).len(), 2);
    }

    /// `x` 的合法目標只有 task 列：worker 列上按 x 必須提示且不開確認框
    /// （§2 selection model）。
    #[test]
    fn x_on_worker_row_is_rejected_with_hint() {
        let m = model();
        let mut app = App::new();
        assert_eq!(handle_key(&mut app, &m, Key::Char('x')), Effect::None);
        assert!(app.confirm.is_none(), "worker 列不得開確認框");
        assert!(app.message.contains("task rows"), "實際：{}", app.message);
    }

    /// task 列上 x → 確認框綁 immutable id；y 執行、n 放棄（§5 單確認）。
    #[test]
    fn x_on_task_row_confirms_then_cancels_by_id() {
        let m = model();
        let mut app = App::new();
        handle_key(&mut app, &m, Key::Char('j')); // w1 → task 列
        handle_key(&mut app, &m, Key::Char('x'));
        assert_eq!(app.confirm.as_deref(), Some("20260731T000001Z-aaaa"));
        // 確認框開著時導航鍵一律吞掉，selection 不動
        assert_eq!(handle_key(&mut app, &m, Key::Char('j')), Effect::None);
        assert_eq!(app.row_idx, 1);
        assert_eq!(
            handle_key(&mut app, &m, Key::Char('y')),
            Effect::Cancel {
                id: "20260731T000001Z-aaaa".into()
            }
        );
        assert!(app.confirm.is_none());

        // n 放棄：不產生 Cancel
        handle_key(&mut app, &m, Key::Char('x'));
        assert!(app.confirm.is_some());
        assert_eq!(handle_key(&mut app, &m, Key::Char('n')), Effect::None);
        assert!(app.confirm.is_none());
    }

    /// Enter：**worker 列** focus 該 pane（P4.6 切片 B 之後 task 列改走 read，
    /// 見 `enter_matrix_dispatches_by_row_kind`）。
    #[test]
    fn enter_focuses_the_pane_of_a_worker_row() {
        let m = model();
        let mut app = App::new();
        assert_eq!(
            handle_key(&mut app, &m, Key::Enter),
            Effect::Focus {
                pane: "%5".into(),
                label: "w1".into()
            }
        );
        // 第三列是 w2（第二列是 w1 的 task）
        handle_key(&mut app, &m, Key::Char('j'));
        handle_key(&mut app, &m, Key::Char('j'));
        assert_eq!(
            handle_key(&mut app, &m, Key::Enter),
            Effect::Focus {
                pane: "%6".into(),
                label: "w2".into()
            }
        );
    }

    /// 導航夾在合法範圍內；q 離開；? 開合法鍵頁、任意鍵關閉。
    #[test]
    fn navigation_clamps_and_meta_keys_work() {
        let m = model();
        let mut app = App::new();
        handle_key(&mut app, &m, Key::Char('k'));
        assert_eq!(app.row_idx, 0, "上緣不得越界");
        for _ in 0..10 {
            handle_key(&mut app, &m, Key::Char('j'));
        }
        assert_eq!(app.row_idx, 2, "下緣夾在最後一列");
        assert_eq!(handle_key(&mut app, &m, Key::Char('?')), Effect::None);
        assert!(app.help);
        handle_key(&mut app, &m, Key::Char('j'));
        assert!(!app.help, "任意鍵關閉合法鍵頁");
        assert_eq!(app.row_idx, 2, "關頁那一鍵不得同時移動 selection");
        assert_eq!(handle_key(&mut app, &m, Key::Char('q')), Effect::Quit);
    }

    /// Tab 兩欄循環：WORKERS → TASKS → WORKERS。DETAIL 不在循環裡
    /// （它只是選中項的投影，§2）。
    #[test]
    fn tab_cycles_two_panels_only() {
        let m = model();
        let mut app = App::new();
        assert_eq!(app.panel, Panel::Workers);
        handle_key(&mut app, &m, Key::Tab);
        assert_eq!(app.panel, Panel::Tasks);
        // ORIGINS 退場後只剩兩欄（P4.7 切片 B）：再按一次就回來
        handle_key(&mut app, &m, Key::Tab);
        assert_eq!(app.panel, Panel::Workers);
    }

    /// `Shift+Tab` 反向循環。兩欄之下它與 `Tab` 走同一個來回——**兩個鍵都
    /// 留著**：手指記得的是「Tab 換欄」，把反向鍵抽掉只會讓人按了沒反應。
    #[test]
    fn backtab_cycles_two_panels_in_reverse() {
        let m = model();
        let mut app = App::new();
        assert_eq!(app.panel, Panel::Workers);
        handle_key(&mut app, &m, Key::BackTab);
        assert_eq!(app.panel, Panel::Tasks);
        handle_key(&mut app, &m, Key::BackTab);
        assert_eq!(app.panel, Panel::Workers);

        // 與 Tab 互為逆運算（兩個起點各驗一次）
        for start in [Panel::Workers, Panel::Tasks] {
            app.panel = start;
            handle_key(&mut app, &m, Key::Tab);
            handle_key(&mut app, &m, Key::BackTab);
            assert_eq!(app.panel, start, "Tab→BackTab MUST 回到原欄：{start:?}");
        }

        // 模態下照樣被吞掉（與 Tab 同紀律：破壞性動作確認期不得換欄）
        app.panel = Panel::Workers;
        app.confirm = Some("20260731T000001Z-aaaa".into());
        assert_eq!(handle_key(&mut app, &m, Key::BackTab), Effect::None);
        assert_eq!(app.panel, Panel::Workers);
        assert!(app.confirm.is_some());
    }

    /// TASKS 欄的 `x`：非終態列開確認框（綁 immutable id）；終態列只提示、
    /// **不開確認框**，訊息須點名「終態」。
    #[test]
    fn x_in_tasks_panel_gates_on_terminal_status() {
        let m = model();
        let mut app = App::new();
        app.panel = Panel::Tasks; // 第 0 列＝completed（反序，新的在上）
        assert_eq!(handle_key(&mut app, &m, Key::Char('x')), Effect::None);
        assert!(app.confirm.is_none(), "終態列不得開確認框");
        assert!(app.message.contains("terminal"), "實際：{}", app.message);

        handle_key(&mut app, &m, Key::Char('j')); // 第 1 列＝queued
        handle_key(&mut app, &m, Key::Char('x'));
        assert_eq!(app.confirm.as_deref(), Some("20260731T000001Z-aaaa"));
    }

    /// `r`／`i` 的合法與非法目標（§3）。
    #[test]
    fn read_and_info_target_rules() {
        let m = model();
        let mut app = App::new();
        // WORKERS 欄的 worker 列：r 無效、i 合法
        assert_eq!(handle_key(&mut app, &m, Key::Char('r')), Effect::None);
        assert!(app.message.contains("task rows"), "實際：{}", app.message);
        assert_eq!(
            handle_key(&mut app, &m, Key::Char('i')),
            Effect::Info {
                worker: "w1".into()
            }
        );
        app.info = None;
        // WORKERS 欄的 task 列：r 合法
        handle_key(&mut app, &m, Key::Char('j'));
        assert_eq!(
            handle_key(&mut app, &m, Key::Char('r')),
            Effect::Read {
                id: "20260731T000001Z-aaaa".into()
            }
        );
        // TASKS 欄的終態列：r 合法（終態才有回覆可讀）
        app.panel = Panel::Tasks;
        assert_eq!(
            handle_key(&mut app, &m, Key::Char('r')),
            Effect::Read {
                id: "20260731T000009Z-dddd".into()
            }
        );
    }

    /// 一份 `n` 行內文的 pager（總列數＝標頭 4 ＋ n ＋ 尾端 2）。
    fn open_pager(app: &mut App, n: usize) {
        let body: String = (0..n).map(|i| format!("line{i}\n")).collect();
        app.pager = Some(Pager {
            id: "20260731T000009Z-dddd".into(),
            from: "alice".into(),
            to: "w1".into(),
            bytes: body.into_bytes(),
            scroll: 0,
        });
    }

    /// overlay（`r` 的 pager）開著時導航鍵只捲動，**MUST NOT** 改動底層
    /// selection——關掉之後人要回得到原本那一列。
    #[test]
    fn pager_swallows_navigation_and_keeps_selection() {
        let m = model();
        let mut app = App::new();
        handle_key(&mut app, &m, Key::Char('j')); // row_idx=1（task 列）
        open_pager(&mut app, 20);
        assert_eq!(handle_key(&mut app, &m, Key::Char('j')), Effect::None);
        assert_eq!(app.row_idx, 1, "overlay 期間底層 selection 不得移動");
        assert_eq!(app.pager.as_ref().unwrap().scroll, 1);
        handle_key(&mut app, &m, Key::Char('k'));
        assert_eq!(app.pager.as_ref().unwrap().scroll, 0);
        handle_key(&mut app, &m, Key::Char('k'));
        assert_eq!(app.pager.as_ref().unwrap().scroll, 0, "上緣不得下溢");
        // q 關 pager 而不是離開程式（模態優先）
        assert_eq!(handle_key(&mut app, &m, Key::Char('q')), Effect::None);
        assert!(app.pager.is_none());
        assert_eq!(app.row_idx, 1);
    }

    /// **F6：pager MUST 吃翻頁鍵。** footer 與 `?` 頁都列著 PgUp／PgDn／
    /// Home／End，畫面上列出的鍵按下去就得有那個效果；它們與 j／k 同族
    /// （捲動語意），且**一樣不得動到底層 selection**。
    #[test]
    fn pager_pages_with_pgup_pgdn_home_end_without_touching_selection() {
        let m = model();
        let mut app = App::new();
        handle_key(&mut app, &m, Key::Char('j')); // row_idx=1
        open_pager(&mut app, 40);
        let page = app.pages.pager as usize; // 預設 10
        let scroll = |a: &App| a.pager.as_ref().unwrap().scroll;
        let max = app.pager.as_ref().unwrap().max_scroll(page);
        assert!(max > page, "前提：這份內容捲得動不只一頁（max={max}）");

        handle_key(&mut app, &m, Key::PageDown);
        assert_eq!(scroll(&app), page, "PgDn 位移一頁");
        handle_key(&mut app, &m, Key::PageUp);
        assert_eq!(scroll(&app), 0, "PgUp 收回同一頁");
        handle_key(&mut app, &m, Key::PageUp);
        assert_eq!(scroll(&app), 0, "上緣不得下溢");

        handle_key(&mut app, &m, Key::End);
        assert_eq!(scroll(&app), max, "End＝最後一頁的頂端");
        // 到底之後再按 PgDn／j 都不得捲進空白（那不是「到底」）
        handle_key(&mut app, &m, Key::PageDown);
        assert_eq!(scroll(&app), max, "下緣夾在最後一頁");
        handle_key(&mut app, &m, Key::Char('j'));
        assert_eq!(scroll(&app), max, "j 同樣夾住");
        handle_key(&mut app, &m, Key::Home);
        assert_eq!(scroll(&app), 0, "Home 回頂");

        assert_eq!(app.row_idx, 1, "翻頁鍵 MUST NOT 動到底層 selection");

        // 內容比一頁短：四個鍵都是 no-op（沒得捲）
        open_pager(&mut app, 2);
        for k in [Key::PageDown, Key::End, Key::Char('j')] {
            handle_key(&mut app, &m, k);
            assert_eq!(scroll(&app), 0, "塞得下就沒得捲（{k:?}）");
        }
    }

    /// `c`：payload 交給 Effect（組裝正本在 action 層），沒有選中列則提示無效。
    /// （P4.7 切片 B：ORIGINS 退場後，「不可 copy」只剩空選取這一格。）
    #[test]
    fn copy_key_emits_payload_for_task_rows() {
        let m = model();
        let mut app = App::new();
        handle_key(&mut app, &m, Key::Char('j')); // task 列
        let Effect::Copy { payload } = handle_key(&mut app, &m, Key::Char('c')) else {
            panic!("task 列按 c 應產生 Copy");
        };
        assert!(payload.contains("agent-bridge read 20260731T000001Z-aaaa"));
        // TASKS 欄空清單＝無選中列
        let mut none = model();
        none.recent.clear();
        app.panel = Panel::Tasks;
        assert_eq!(handle_key(&mut app, &none, Key::Char('c')), Effect::None);
    }

    /// `e` 的合法目標只有 worker 列（§3／§5：破壞性動作要有唯一明確目標）；
    /// WORKERS 欄的 task 列與 TASKS 欄一律提示無效、不開證據框。
    #[test]
    fn evict_key_targets_worker_rows_only() {
        let m = model();
        let mut app = App::new();
        assert_eq!(
            handle_key(&mut app, &m, Key::Char('e')),
            Effect::EvictPrompt {
                worker: "w1".into()
            }
        );
        assert!(app.evict_prompt.is_none(), "讀 registry 之前不開框");

        handle_key(&mut app, &m, Key::Char('j')); // task 列
        assert_eq!(handle_key(&mut app, &m, Key::Char('e')), Effect::None);
        assert!(app.message.contains("worker rows"), "實際：{}", app.message);

        app.panel = Panel::Tasks;
        assert_eq!(handle_key(&mut app, &m, Key::Char('e')), Effect::None);
    }

    /// in-flight 閘：同一個 worker 的 evict 還在跑時，再按 e MUST 只提示
    /// ——一次性 thread 各自獨立，沒有這道閘就會有兩輪收尾任務同時派出去。
    #[test]
    fn evict_key_is_blocked_while_one_is_in_flight() {
        let m = model();
        let mut app = App::new();
        app.evict_inflight.insert("w1".to_string());
        assert_eq!(handle_key(&mut app, &m, Key::Char('e')), Effect::None);
        assert!(app.message.contains("in progress"), "實際：{}", app.message);
        assert!(app.evict_prompt.is_none());
        // 別的 worker 不受影響（第三列＝w2）
        handle_key(&mut app, &m, Key::Char('j'));
        handle_key(&mut app, &m, Key::Char('j'));
        assert_eq!(
            handle_key(&mut app, &m, Key::Char('e')),
            Effect::EvictPrompt {
                worker: "w2".into()
            }
        );
    }

    /// 證據框的模態紀律（同 `x`）：y／Enter 執行且**只帶框上顯示過的值**，
    /// n／Esc 放棄，其餘鍵一律吞掉（導航鍵不得在破壞性模態下改變 selection）。
    #[test]
    fn evict_prompt_is_modal_and_carries_shown_generation() {
        let m = model();
        let mut app = App::new();
        let shown = crate::action::EvictShown {
            name: "w1".into(),
            pane: "%5".into(),
            spawn_tag: "t-gen1".into(),
        };
        let prompt = || crate::action::EvictPrompt {
            shown: shown.clone(),
            lines: vec!["wrap-up task, then reclaim".into()],
        };

        app.evict_prompt = Some(prompt());
        assert_eq!(handle_key(&mut app, &m, Key::Char('j')), Effect::None);
        assert_eq!(app.row_idx, 0, "模態下 selection 不得移動");
        assert!(app.evict_prompt.is_some(), "其餘鍵不得關框");
        assert_eq!(
            handle_key(&mut app, &m, Key::Char('y')),
            Effect::Evict {
                shown: shown.clone()
            }
        );
        assert!(app.evict_prompt.is_none());

        app.evict_prompt = Some(prompt());
        assert_eq!(handle_key(&mut app, &m, Key::Esc), Effect::None);
        assert!(app.evict_prompt.is_none());
        assert!(
            app.message.contains("evict aborted"),
            "實際：{}",
            app.message
        );
    }

    /// 警告是 sticky 的有界歷史（major #2）：**append 不覆寫**、連續重複只留
    /// 一則、滿了丟最舊的、只有人按 Esc 才清得掉。
    #[test]
    fn warnings_accumulate_and_are_only_cleared_by_the_human() {
        let m = model();
        let mut app = App::new();
        app.push_warning("無法通知 w1".into());
        app.message = "evict：收尾任務已派出，等待筆記落地".to_string();
        assert_eq!(
            app.warnings,
            vec!["無法通知 w1".to_string()],
            "message 的覆寫 MUST NOT 影響警告"
        );

        // 連續重複只留一則
        app.push_warning("無法通知 w1".into());
        assert_eq!(app.warnings.len(), 1);

        // 上限：丟最舊的（新的才是人此刻要處理的）
        for i in 0..MAX_WARNINGS + 2 {
            app.push_warning(format!("w{i}"));
        }
        assert_eq!(app.warnings.len(), MAX_WARNINGS);
        assert_eq!(
            app.warnings.last().unwrap(),
            &format!("w{}", MAX_WARNINGS + 1)
        );
        assert!(
            !app.warnings.iter().any(|w| w == "無法通知 w1"),
            "溢位時丟的是最舊的"
        );

        // 導航／其他鍵不清警告
        handle_key(&mut app, &m, Key::Char('j'));
        assert_eq!(app.warnings.len(), MAX_WARNINGS);
        // Esc（無 overlay）＝人說「我看過了」
        assert_eq!(handle_key(&mut app, &m, Key::Esc), Effect::None);
        assert!(app.warnings.is_empty());
        assert!(app.message.contains("cleared"), "實際：{}", app.message);
    }

    /// 列表縮短（極端到整個清空）後 relocate 把 selection 夾回合法範圍
    /// （500ms 重讀路徑）。
    #[test]
    fn relocate_after_reload_keeps_selection_valid() {
        let m = model();
        let mut app = App::new();
        app.row_idx = 2;
        app.sync_keys(&m);
        let empty = Model {
            groups: Vec::new(),
            workers: Vec::new(),
            tasks: Vec::new(),
            recent: Vec::new(),
            recent_truncated: false,
        };
        app.relocate(&empty);
        assert_eq!(app.row_idx, 0, "一列都沒有時索引不得越界");
        assert!(app.selected_row(&empty).is_none());
        // 同一批列回來（例如某一輪 registry 讀空）→ **回到原本那一項**：
        // stable key 在空快照那一輪被保留著，這正是它存在的理由
        app.relocate(&m);
        assert_eq!(app.row_idx, 2);
        let Some(Row::Worker(wi)) = app.selected_row(&m) else {
            panic!("應回到 worker 列");
        };
        assert_eq!(m.workers[wi].name, "w2");
    }

    // ---- P4.6 切片 B ----

    /// Enter matrix 的三個分支（§9 P4.6；P4.7 切片 B 拿掉 ORIGINS 那一格）。
    #[test]
    fn enter_matrix_dispatches_by_row_kind() {
        let m = model();

        // (1) worker 列 → focus 該 pane（語意不變）
        let mut app = App::new();
        assert_eq!(
            handle_key(&mut app, &m, Key::Enter),
            Effect::Focus {
                pane: "%5".into(),
                label: "w1".into()
            }
        );

        // (2) WORKERS 的內嵌 task 列 → read 該 task，**不再 focus 它的 worker**
        handle_key(&mut app, &m, Key::Char('j'));
        assert_eq!(
            handle_key(&mut app, &m, Key::Enter),
            Effect::Read {
                id: "20260731T000001Z-aaaa".into()
            },
            "task 列的 Enter MUST 是 read，不得 focus 所屬 worker"
        );

        // (3) TASKS 列 → read
        app.panel = Panel::Tasks;
        assert_eq!(
            handle_key(&mut app, &m, Key::Enter),
            Effect::Read {
                id: "20260731T000009Z-dddd".into()
            }
        );
    }

    /// 空清單上的 Enter：不動、不報錯（P4.7 切片 B 取代原本的
    /// `enter_on_empty_scope_stays_put_without_error`——沒有 scope 可切了，
    /// 但「Enter 落在無選中列上是 no-op」這條語意要留著）。
    #[test]
    fn enter_on_an_empty_list_is_a_no_op() {
        let m = Model {
            groups: Vec::new(),
            workers: Vec::new(),
            tasks: Vec::new(),
            recent: Vec::new(),
            recent_truncated: false,
        };
        let mut app = App::new();
        for panel in [Panel::Workers, Panel::Tasks] {
            app.panel = panel;
            app.sync_keys(&m);
            assert_eq!(handle_key(&mut app, &m, Key::Enter), Effect::None);
            assert_eq!(app.panel, panel, "無選中列時焦點不得跳走");
            assert_eq!(row_caps(&app, &m).enter, EnterAct::None);
        }
    }

    /// `r` ≡ task 列的 `Enter`：同一個 id、同一個 Effect（alias 而非兩份實作）。
    #[test]
    fn r_is_an_exact_alias_of_enter_on_task_rows() {
        let m = model();
        for panel in [Panel::Workers, Panel::Tasks] {
            let mut a = App::new();
            a.panel = panel;
            if panel == Panel::Workers {
                handle_key(&mut a, &m, Key::Char('j')); // 走到 task 列
            }
            let mut b = App::new();
            b.panel = panel;
            if panel == Panel::Workers {
                handle_key(&mut b, &m, Key::Char('j'));
            }
            let via_enter = handle_key(&mut a, &m, Key::Enter);
            let via_r = handle_key(&mut b, &m, Key::Char('r'));
            assert!(matches!(via_enter, Effect::Read { .. }));
            assert_eq!(via_enter, via_r, "{panel:?}：Enter 與 r MUST 同一結果");
        }
    }

    /// stable selection（1）：在選取項**之前**插入一列，游標仍指原本那一項
    /// ——人沒動選取，reload MUST NOT 跳列。
    #[test]
    fn relocate_follows_the_item_when_rows_are_inserted_above() {
        let m = model();
        let mut app = App::new();
        handle_key(&mut app, &m, Key::Char('j'));
        handle_key(&mut app, &m, Key::Char('j')); // 第 2 列＝w2
        assert!(matches!(app.selected_row(&m), Some(Row::Worker(1))));

        // 新 worker `w0` 排在字典序最前（snapshot 序＝檔名序）
        let mut m2 = model();
        m2.workers.insert(
            0,
            AgentSnapshot {
                name: "w0".into(),
                pane: "%4".into(),
                runtime: "codex".into(),
                owner: "it:@1".into(),
                ready: "ready".into(),
                spawn_tag: "t-gen1".into(),
                registered_at: "2026-07-31T00:00:00Z".into(),
                spawned: true,
                corrupt: false,
                // P4.7 切片 A：lineage 兩欄對這些 fixture 無關（None＝欄位缺席）
                lineage_root: None,
                parent_agent: None,
            },
        );
        let m2 = with_groups(m2);
        app.relocate(&m2);
        assert_eq!(app.row_idx, 3, "列序後移一格，游標要跟著走");
        let Some(Row::Worker(wi)) = app.selected_row(&m2) else {
            panic!("仍應停在 worker 列");
        };
        assert_eq!(m2.workers[wi].name, "w2", "選取的仍是同一個 worker");
    }

    /// **P4.7 切片 B1：分組把列序整個重排，stable key 仍認人不認位置。**
    ///
    /// 分組是 P4.6 之後第一個會**大幅**動到列序的東西（新 spawn 出來的一整條
    /// lineage 會整組插在 standalone 段之前）。沒有這條，`row_idx` 的位置快取
    /// 一旦被當成權威，畫面就會在別人 spawn 的那一輪把人的游標甩到別的 worker
    /// 身上——而那正是 stable key 存在的理由。
    #[test]
    fn selection_survives_a_wholesale_regroup() {
        let m = model(); // w1／w2 都是 standalone（沒有 lineage 欄）
        let mut app = App::new();
        handle_key(&mut app, &m, Key::Char('j'));
        handle_key(&mut app, &m, Key::Char('j')); // 第 2 列＝w2
        assert!(matches!(app.selected_row(&m), Some(Row::Worker(1))));

        // 另一條 lineage 冒出來：它排在 standalone 段**之前**，w1／w2 整段後移
        let root = "AGENT_BRIDGE_SPAWN_TAG=ab-spawn-root-1-aaaaaaaaaaaa";
        let mut m2 = model();
        let kid = "AGENT_BRIDGE_SPAWN_TAG=ab-spawn-b-2-bbbbbbbbbbbb";
        for (i, (name, pane, tag)) in [("a", "%81", root), ("b", "%82", kid)].iter().enumerate() {
            m2.workers.insert(
                i,
                AgentSnapshot {
                    name: (*name).into(),
                    pane: (*pane).into(),
                    runtime: "codex".into(),
                    owner: "it:@1".into(),
                    ready: "ready".into(),
                    spawn_tag: (*tag).into(),
                    registered_at: "2026-07-31T00:00:00Z".into(),
                    spawned: true,
                    corrupt: false,
                    lineage_root: Some(root.to_string()),
                    parent_agent: (i == 1).then(|| root.to_string()),
                },
            );
        }
        let m2 = with_groups(m2);
        assert_eq!(m2.groups.len(), 2, "前提：真的分成兩組了");

        app.relocate(&m2);
        let Some(Row::Worker(wi)) = app.selected_row(&m2) else {
            panic!("仍應停在 worker 列");
        };
        assert_eq!(
            m2.workers[wi].name, "w2",
            "重排 MUST NOT 把游標甩到別的 worker（實際 row_idx={}）",
            app.row_idx
        );
    }

    /// stable selection（2）：選取項消失 → 落**前一列**（無則後一列、再無則
    /// 第 0 列）。三型 key 各驗一次。
    #[test]
    fn relocate_falls_back_to_the_neighbour_row() {
        // worker：選 w2（第 2 列），w2 從 registry 消失
        let m = model();
        let mut app = App::new();
        handle_key(&mut app, &m, Key::Char('j'));
        handle_key(&mut app, &m, Key::Char('j'));
        let mut m2 = model();
        m2.workers.remove(1); // 拿掉 w2
        let m2 = with_groups(m2);
        app.relocate(&m2);
        assert_eq!(app.row_idx, 1, "落前一列（w1 的 task 列）");

        // task：TASKS 欄選第 1 列，該 task 從 recent 消失
        let mut app = App::new();
        app.panel = Panel::Tasks;
        handle_key(&mut app, &m, Key::Char('j'));
        assert_eq!(app.task_key.as_deref(), Some("20260731T000001Z-aaaa"));
        let mut m3 = model();
        m3.recent.remove(1);
        app.relocate(&m3);
        assert_eq!(app.task_idx, 0, "落前一列");
    }

    /// stable selection（3）：同名 respawn 換了 `spawn_tag` ＝**新的一代**，
    /// 不是原本那一項——選取 MUST NOT 無聲接續過去（世代是 evict CAS 的比對
    /// 軸，接續等於把破壞性動作指向另一個對象）。
    #[test]
    fn respawn_with_a_new_generation_is_not_the_same_row() {
        let m = model();
        let mut app = App::new();
        handle_key(&mut app, &m, Key::Char('j'));
        handle_key(&mut app, &m, Key::Char('j')); // w2（row 2）

        let mut m2 = model();
        m2.workers[1].spawn_tag = "t-gen2".into(); // w2 換代
        let m2 = with_groups(m2);
        app.relocate(&m2);
        assert_eq!(app.row_idx, 1, "換代＝找不到原項，落前一列");
        assert!(
            !matches!(app.row_key, Some(RowKey::Worker { ref spawn_tag, .. }) if spawn_tag == "t-gen2"),
            "MUST NOT 把選取接續到新一代身上"
        );
    }

    /// **審查 major 的 regression**：選中列在 **index 0** 換代。
    ///
    /// 舊版的鄰列回退是 `neighbor(0, len) == 0`，而 index 0 正是那個同名新一代
    /// 所在的位置——於是 selection 無聲接續到新世代，`e`／DETAIL 的目標跟著換
    /// 人。這裡鎖死：MUST NOT 落在新一代身上。
    #[test]
    fn a_respawn_at_index_zero_never_inherits_the_selection() {
        let m = model();
        let mut app = App::new();
        assert!(
            matches!(app.selected_row(&m), Some(Row::Worker(0))),
            "前提：選中第 0 列"
        );
        app.sync_keys(&m);

        // w1 換代（名字與位置都不變，只有 spawn_tag 變了）
        let mut m2 = model();
        m2.workers[0].spawn_tag = "t-gen2".into();
        let m2 = with_groups(m2);
        app.relocate(&m2);
        assert!(
            !matches!(app.selected_row(&m2), Some(Row::Worker(0))),
            "MUST NOT 落在同名新一代身上（實際 row_idx={}）",
            app.row_idx
        );
        // 這份 fixture 的第 1 列是 w1 的 task 列（immutable id，不受換代影響）
        assert!(matches!(app.selected_row(&m2), Some(Row::Task { .. })));

        // 破壞性動作也不得指向新一代
        let mut app2 = App::new();
        app2.sync_keys(&m);
        app2.relocate(&m2);
        assert_eq!(handle_key(&mut app2, &m2, Key::Char('e')), Effect::None);
    }

    /// 同上，但**旁邊一列都沒有**（換代那一列是唯一的列）：明確不選任何列
    /// ＋一則訊息，且這個狀態要停得住——按過無關的鍵、再跑幾輪 reload，都
    /// MUST NOT 自己選回新一代。
    #[test]
    fn a_lone_respawn_clears_the_selection_and_stays_cleared() {
        let one = |tag: &str| {
            with_groups(Model {
                groups: Vec::new(),
                workers: vec![AgentSnapshot {
                    name: "w1".into(),
                    pane: "%5".into(),
                    runtime: "codex".into(),
                    owner: "it:@1".into(),
                    ready: "ready".into(),
                    spawn_tag: tag.into(),
                    registered_at: "2026-07-31T00:00:00Z".into(),
                    spawned: true,
                    corrupt: false,
                    // P4.7 切片 A：lineage 兩欄對這些 fixture 無關（None＝欄位缺席）
                    lineage_root: None,
                    parent_agent: None,
                }],
                tasks: Vec::new(),
                recent: Vec::new(),
                recent_truncated: false,
            })
        };
        let m = one("t-gen1");
        let mut app = App::new();
        app.sync_keys(&m);
        assert!(app.selected_row(&m).is_some());

        let m2 = one("t-gen2");
        app.relocate(&m2);
        assert!(app.selected_row(&m2).is_none(), "MUST 明確不選任何列");
        assert!(
            app.message.contains("new generation"),
            "要說明為什麼沒有選取，實際：{}",
            app.message
        );
        // `e`／`x` 在無選取下一律拒絕（不得誤指新一代）
        assert_eq!(handle_key(&mut app, &m2, Key::Char('e')), Effect::None);
        assert!(app.evict_prompt.is_none());

        // 無關的按鍵（`?` 開關頁）＋數輪 reload 之後，仍然沒有選取
        handle_key(&mut app, &m2, Key::Char('?'));
        handle_key(&mut app, &m2, Key::Char('?'));
        for _ in 0..3 {
            app.relocate(&m2);
        }
        assert!(app.selected_row(&m2).is_none(), "這個狀態 MUST 停得住");

        // 人按 j 就重新選得回來（不是死局）
        handle_key(&mut app, &m2, Key::Char('j'));
        assert!(app.selected_row(&m2).is_some());
        assert_eq!(
            app.row_key,
            Some(crate::model::row_key(&m2, Row::Worker(0)))
        );
    }

    // ---- P4.6 切片 C：翻頁鍵 ----

    /// n 個 worker、沒有 task 的模型（WORKERS 欄剛好 n 列，翻頁算術驗得乾淨）。
    fn many(n: usize) -> Model {
        with_groups(Model {
            groups: Vec::new(),
            workers: (0..n)
                .map(|i| AgentSnapshot {
                    name: format!("w{i:02}"),
                    pane: format!("%{i}"),
                    runtime: "codex".into(),
                    owner: "it:@1".into(),
                    ready: "ready".into(),
                    spawn_tag: "t-gen1".into(),
                    registered_at: "2026-07-31T00:00:00Z".into(),
                    spawned: true,
                    corrupt: false,
                    // P4.7 切片 A：lineage 兩欄對這些 fixture 無關（None＝欄位缺席）
                    lineage_root: None,
                    parent_agent: None,
                })
                .collect(),
            tasks: Vec::new(),
            recent: Vec::new(),
            recent_truncated: false,
        })
    }

    /// n 個**單成員 lineage**（每組一行標頭＋一列 worker，無 in-flight task）
    /// ——G2 的失敗情境靠的就是「標頭與列一比一交錯」。
    fn lineages(n: usize) -> Model {
        let key = |t: &str| format!("AGENT_BRIDGE_SPAWN_TAG=ab-spawn-{t}");
        with_groups(Model {
            groups: Vec::new(),
            workers: (0..n)
                .map(|i| {
                    let tag = key(&format!("g{i:02}-{i}-{:012x}", i + 1));
                    AgentSnapshot {
                        name: format!("g{i:02}"),
                        pane: format!("%{i}"),
                        runtime: "codex".into(),
                        owner: "it:@1".into(),
                        ready: "ready".into(),
                        spawn_tag: tag.clone(),
                        registered_at: "2026-07-31T00:00:00Z".into(),
                        spawned: true,
                        corrupt: false,
                        // 自己就是自己那一組的根（無 parent 的新式列的合法形狀）
                        lineage_root: Some(tag),
                        parent_agent: None,
                    }
                })
                .collect(),
            tasks: Vec::new(),
            recent: Vec::new(),
            recent_truncated: false,
        })
    }

    fn pages(workers: u16, tasks: u16) -> PageSizes {
        PageSizes {
            workers,
            tasks,
            ..PageSizes::default()
        }
    }

    /// PgUp／PgDn 位移一頁（＝該面板可視高度）、Home／End 到頭到尾，
    /// 兩端一律飽和不 wrap。
    #[test]
    fn page_keys_move_by_one_viewport_and_saturate_at_both_ends() {
        let m = many(30);
        let mut app = App::new();
        app.pages = pages(8, 4);

        handle_key(&mut app, &m, Key::PageDown);
        assert_eq!(app.row_idx, 8, "一頁＝WORKERS 的可視高度");
        handle_key(&mut app, &m, Key::PageDown);
        assert_eq!(app.row_idx, 16);
        for _ in 0..10 {
            handle_key(&mut app, &m, Key::PageDown);
        }
        assert_eq!(app.row_idx, 29, "下緣飽和，MUST NOT wrap 回頂端");
        handle_key(&mut app, &m, Key::PageUp);
        assert_eq!(app.row_idx, 21);
        handle_key(&mut app, &m, Key::Home);
        assert_eq!(app.row_idx, 0);
        handle_key(&mut app, &m, Key::PageUp);
        assert_eq!(app.row_idx, 0, "上緣飽和");
        handle_key(&mut app, &m, Key::End);
        assert_eq!(app.row_idx, 29);

        // 每個面板用**自己**的高度：TASKS 一頁 4 列，與 WORKERS 的 8 不同
        app.panel = Panel::Tasks;
        handle_key(&mut app, &m, Key::PageDown);
        assert_eq!(app.task_idx, 0, "這份 fixture 沒有 recent task");
        assert_eq!(app.row_idx, 29, "換面板不得動到 WORKERS 的位置");
    }

    /// **G2 回歸（切片 B1 修正輪）：翻頁 MUST NOT 跳列。**
    ///
    /// codex 的失敗情境：可視行高 8、九個單成員 lineage（每組一行標頭＋一列
    /// worker）。把行高直接當列數用的話，一次 PgDown 從 row 0 跳到 row 8，
    /// 而 row 4 在**任何一頁上都不會出現**——它既不在第一頁（行 0–7 只到
    /// row 3），也不在新的一頁（行 16 起）。
    ///
    /// 正確語意：位移量是一個 viewport 的 **rendered lines**，落點是行號跨過
    /// 該位置的第一個列——於是下一頁的頁首恰好接上一頁的末列。
    #[test]
    fn paging_across_group_headers_never_skips_a_row() {
        let m = lineages(9);
        // 行號：標頭 0、row0=1、標頭 2、row1=3、…、row_i = 2i+1
        let nof = crate::model::Filter::default();
        assert_eq!(crate::model::worker_line_of(&m, &nof, 0), 1);
        assert_eq!(crate::model::worker_line_of(&m, &nof, 4), 9);
        let mut app = App::new();
        app.pages = pages(8, 8);

        handle_key(&mut app, &m, Key::PageDown);
        assert_eq!(
            app.row_idx, 4,
            "第一頁畫得下 row 0–3（行 0–7），下一頁 MUST 從 row 4 起"
        );
        handle_key(&mut app, &m, Key::PageDown);
        assert_eq!(app.row_idx, 8);
        // 反向對稱：翻回去落在同一批頁首上
        handle_key(&mut app, &m, Key::PageUp);
        assert_eq!(app.row_idx, 4);
        handle_key(&mut app, &m, Key::PageUp);
        assert_eq!(app.row_idx, 0);
    }

    /// 同一件事的 sweep 版：一路 PgDown 到底、再一路 PgUp 回頂，把每一頁畫得
    /// 出來的列聯集起來——**每一列都必須出現過至少一次**。
    ///
    /// 上一條釘的是特定落點，這條釘的是「沒有任何一列被漏掉」這個性質本身
    /// （改成別的位移公式只要漏一列就紅）。
    #[test]
    fn a_full_page_sweep_covers_every_row_in_both_directions() {
        for n in [3usize, 9, 17] {
            for h in [1u16, 2, 3, 8] {
                let m = lineages(n);
                let mut app = App::new();
                app.pages = pages(h, h);
                let lines = crate::model::worker_row_lines(&m, &crate::model::Filter::default());
                assert_eq!(lines.len(), n, "每個單成員組恰一列");
                let mut seen = vec![false; n];
                // 每一頁看得到的列＝行號落在 [scroll, scroll+h) 的那些列
                let mut mark = |app: &App| {
                    let sel = lines[app.row_idx];
                    let top = sel.saturating_sub(usize::from(h).saturating_sub(1));
                    for (r, &l) in lines.iter().enumerate() {
                        if l >= top && l < top + usize::from(h).max(1) {
                            seen[r] = true;
                        }
                    }
                };
                mark(&app);
                for _ in 0..(2 * n + 4) {
                    handle_key(&mut app, &m, Key::PageDown);
                    mark(&app);
                }
                assert_eq!(app.row_idx, n - 1, "一路翻到底（n={n} h={h}）");
                for _ in 0..(2 * n + 4) {
                    handle_key(&mut app, &m, Key::PageUp);
                    mark(&app);
                }
                assert_eq!(app.row_idx, 0, "一路翻回頂（n={n} h={h}）");
                let missed: Vec<usize> = (0..n).filter(|&r| !seen[r]).collect();
                assert!(
                    missed.is_empty(),
                    "n={n} h={h}：這些列在任何一頁上都沒出現過 {missed:?}"
                );
            }
        }
    }

    /// 列數**不足一頁**時，翻頁等於到底／到頂（不越界）。
    #[test]
    fn page_keys_on_a_short_list_land_on_the_edges() {
        let m = many(3);
        let mut app = App::new();
        app.pages = pages(8, 8);
        handle_key(&mut app, &m, Key::PageDown);
        assert_eq!(app.row_idx, 2);
        handle_key(&mut app, &m, Key::PageUp);
        assert_eq!(app.row_idx, 0);
    }

    /// **空面板**按這四鍵：不動、不 panic、不報錯（一則訊息都不留）。
    #[test]
    fn page_keys_on_an_empty_panel_do_nothing() {
        let m = many(0);
        let mut app = App::new();
        for k in [Key::PageDown, Key::PageUp, Key::Home, Key::End] {
            assert_eq!(handle_key(&mut app, &m, k), Effect::None);
            assert_eq!(app.row_idx, 0);
            assert!(app.message.is_empty(), "實際：{}", app.message);
        }
        // TASKS 欄同樣（fixture 沒有任何 recent）
        app.panel = Panel::Tasks;
        for k in [Key::PageDown, Key::End] {
            assert_eq!(handle_key(&mut app, &m, k), Effect::None);
            assert_eq!(app.task_idx, 0);
        }
    }

    /// 翻頁走的是與 j／k 同一條路徑：位移後 selection 仍是 **stable key**，
    /// 下一輪 reload 重排也不跳列（切片 B 的不變量不得被切片 C 旁路）。
    #[test]
    fn paging_keeps_selection_on_a_stable_key() {
        let m = many(30);
        let mut app = App::new();
        app.pages = pages(8, 4);
        handle_key(&mut app, &m, Key::PageDown);
        let Some(Row::Worker(wi)) = app.selected_row(&m) else {
            panic!("應選在 worker 列");
        };
        let name = m.workers[wi].name.clone();
        assert_eq!(
            app.row_key,
            Some(RowKey::Worker {
                name: name.clone(),
                spawn_tag: "t-gen1".into()
            })
        );

        // 在選取之前插入一列 → 位置變、選的還是同一個 worker
        let mut m2 = many(30);
        m2.workers.insert(
            0,
            AgentSnapshot {
                name: "w--".into(),
                pane: "%99".into(),
                runtime: "codex".into(),
                owner: "it:@1".into(),
                ready: "ready".into(),
                spawn_tag: "t-gen1".into(),
                registered_at: "2026-07-31T00:00:00Z".into(),
                spawned: true,
                corrupt: false,
                // P4.7 切片 A：lineage 兩欄對這些 fixture 無關（None＝欄位缺席）
                lineage_root: None,
                parent_agent: None,
            },
        );
        let m2 = with_groups(m2);
        app.relocate(&m2);
        assert_eq!(app.row_idx, 9, "列序後移一格");
        let Some(Row::Worker(wi2)) = app.selected_row(&m2) else {
            panic!("仍應在 worker 列");
        };
        assert_eq!(m2.workers[wi2].name, name);
    }

    /// 破壞性動作的模態下，翻頁鍵與導航鍵同紀律：一律吞掉、selection 不動。
    #[test]
    fn page_keys_are_swallowed_by_the_confirm_modal() {
        let m = model();
        let mut app = App::new();
        handle_key(&mut app, &m, Key::Char('j'));
        handle_key(&mut app, &m, Key::Char('x'));
        assert!(app.confirm.is_some());
        assert_eq!(handle_key(&mut app, &m, Key::PageDown), Effect::None);
        assert_eq!(app.row_idx, 1, "模態下 selection 不得移動");
        assert!(app.confirm.is_some(), "翻頁鍵不得關掉確認框");
    }

    // ── P4.7 切片 D：`L` 尾行預覽 ─────────────────────────────────────

    fn capture(text: &str, truncated: bool) -> ab_core::tmux::TailCapture {
        ab_core::tmux::TailCapture {
            text: text.to_string(),
            truncated,
        }
    }

    /// `L` 的合法目標只有**有 pane 的 worker 列**，而且提示與 dispatch 讀的是
    /// 同一份 `RowCaps`——footer 說按得動、按下去卻回一行拒絕訊息，正是
    /// contextual footer 要消滅的東西。
    #[test]
    fn peek_only_acts_on_worker_rows_that_have_a_pane() {
        let m = model();
        let mut app = App::new();
        // 第 0 列＝w1（pane %5）
        assert!(row_caps(&app, &m).peek, "有 pane 的 worker 列 MUST 可按");
        let Effect::Peek { target } = handle_key(&mut app, &m, Key::Char('L')) else {
            panic!("worker 列上的 `L` MUST 產出 Peek");
        };
        assert_eq!(target.pane, "%5");
        assert_eq!(target.name, "w1");

        // task 列：沒有唯一的 pane 可言
        let mut app = App::new();
        handle_key(&mut app, &m, Key::Char('j'));
        assert!(matches!(app.selection(&m), Sel::Task { .. }));
        assert!(!row_caps(&app, &m).peek);
        assert_eq!(handle_key(&mut app, &m, Key::Char('L')), Effect::None);
        assert!(app.message.contains("worker rows that have a pane"));

        // pane 欄空的 worker 列：capture 不到任何東西
        let mut m2 = model();
        m2.workers[0].pane = String::new();
        let m2 = with_groups(m2);
        let mut app = App::new();
        assert!(!row_caps(&app, &m2).peek);
        assert_eq!(handle_key(&mut app, &m2, Key::Char('L')), Effect::None);
        assert!(peek_target(&app, &m2).is_none(), "caps 與目標同一個條件");
    }

    /// **一次按鍵一個 request**：閘沒放開之前再按 `L` 不再送。
    #[test]
    fn peek_sends_one_request_per_keypress() {
        let m = model();
        let mut app = App::new();
        let Effect::Peek { target } = handle_key(&mut app, &m, Key::Char('L')) else {
            panic!("第一次 MUST 送出");
        };
        // run loop 起 thread 成功＝掛上閘（見 `crate::start_peek`）
        app.peek_inflight = Some(target.clone());
        assert_eq!(
            handle_key(&mut app, &m, Key::Char('L')),
            Effect::None,
            "在途期間 MUST NOT 再送第二個 request"
        );
        assert!(
            app.message.contains("already in progress"),
            "{}",
            app.message
        );

        // 回信一到，閘就放開——**失敗也放開**，否則失敗一次就再也按不動
        peek_apply(&mut app, &m, &target, None);
        assert!(app.peek_inflight.is_none());
        assert!(
            matches!(
                handle_key(&mut app, &m, Key::Char('L')),
                Effect::Peek { .. }
            ),
            "放閘之後 MUST 能再送"
        );
    }

    /// 結果回來時 selection 還在原處：貼上去、標記截斷。
    #[test]
    fn a_peek_result_lands_on_the_row_that_asked_for_it() {
        let m = model();
        let mut app = App::new();
        let Effect::Peek { target } = handle_key(&mut app, &m, Key::Char('L')) else {
            panic!("MUST 送出");
        };
        app.peek_inflight = Some(target.clone());
        peek_apply(&mut app, &m, &target, Some(capture("a\nb\n\n\n", true)));
        let p = app.peek.as_ref().expect("MUST 開出預覽");
        assert!(p.title.contains("w1") && p.title.contains("%5"));
        assert_eq!(p.lines, vec!["a", "b"], "尾端補出來的空行不是內容");
        assert!(p.truncated, "截斷 MUST 傳到呈現層（畫面要標記）");
        // 任意鍵關閉，且**不取消在途請求**（閘由回信放開，不是由關窗放開）
        app.peek_inflight = Some(target);
        assert_eq!(handle_key(&mut app, &m, Key::Char('j')), Effect::None);
        assert!(app.peek.is_none());
        assert!(app.peek_inflight.is_some());
    }

    /// **晚到的結果不貼新 selection**：request 帶著目標識別出門，回來時比對，
    /// 不符就丟棄——而且要說出來，否則人會以為 `L` 沒作用。
    #[test]
    fn a_late_peek_result_is_discarded_not_pasted_on_the_new_row() {
        let m = model();
        let mut app = App::new();
        let Effect::Peek { target } = handle_key(&mut app, &m, Key::Char('L')) else {
            panic!("MUST 送出");
        };
        app.peek_inflight = Some(target.clone());

        // (1) 人換到了別的列
        handle_key(&mut app, &m, Key::Char('j'));
        peek_apply(&mut app, &m, &target, Some(capture("late", false)));
        assert!(app.peek.is_none(), "MUST NOT 貼到新 selection 上");
        assert!(app.message.contains("discarded"), "{}", app.message);
        assert!(app.peek_inflight.is_none(), "閘照樣放開");

        // (2) 列還在原處，但那一列已經**換代**（同名 respawn）：pane 相同也算
        // 不同目標——世代是 evict CAS 的比對軸，預覽不得比它寬鬆
        let mut app = App::new();
        let Effect::Peek { target } = handle_key(&mut app, &m, Key::Char('L')) else {
            panic!("MUST 送出");
        };
        let mut m2 = model();
        m2.workers[0].spawn_tag = "t-gen2".into();
        let m2 = with_groups(m2);
        app.relocate(&m2);
        peek_apply(&mut app, &m2, &target, Some(capture("late", false)));
        assert!(app.peek.is_none(), "換代之後 MUST NOT 貼上");
    }

    /// 人沒動選取時，連跑幾輪 relocate（模型未變）MUST 完全不動。
    #[test]
    fn repeated_relocate_is_idempotent_when_nothing_changed() {
        let m = model();
        let mut app = App::new();
        handle_key(&mut app, &m, Key::Char('j'));
        let (r, t) = (app.row_idx, app.task_idx);
        for _ in 0..3 {
            app.relocate(&m);
        }
        assert_eq!((app.row_idx, app.task_idx), (r, t));
    }
}
