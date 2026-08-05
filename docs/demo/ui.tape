# README「ui dashboard」情境 GIF。從 repo 根目錄執行：
#   vhs docs/demo/ui.tape
# 依賴：vhs、ttyd、ffmpeg、tmux、jq。全程走 demo stub，不打真實 API。
# 賣點：兩條 lineage、兩個任務在跑，其中一個 worker 停在權限框等人——
# dashboard 把那一組浮到最上面、在組標頭標 ⛔、在 DETAIL 直接顯示框在問
# 什麼；Enter 一鍵跳到那個 pane。全程不離開畫面就答得出「誰卡住、卡在哪」。
Output docs/assets/ui.gif

Set Shell bash
Set FontSize 15
Set Width 1300
Set Height 720
Set Padding 12
Set TypingSpeed 40ms

Hide
Type "./docs/demo/setup.sh docs/demo/ui-driver.sh && tmux -L ab-demo attach"
Enter
Sleep 4
Show

Sleep 1
Type "# two workers, two tasks in flight"
Enter
Sleep 1.2
Type "agent-bridge spawn api-lead --runtime codex"
Enter
Sleep 5
Type "agent-bridge spawn docs-lead --runtime codex"
Enter
Sleep 5

Type "agent-bridge send api-lead --from main --message 'Benchmark the task index; report p99.'"
Enter
Sleep 3
Type "agent-bridge send docs-lead --from main --message 'Rewrite the quickstart for the new flags.'"
Enter
Sleep 4

Type "# one of them is now waiting on a permission prompt — open the dashboard"
Enter
Sleep 2

# 先 zoom：worker pane 就在同一個 window 裡，不讓出整個寬度的話 dashboard
# 只有半個畫面，兩行列的摘要會被截掉——那不是它平常的樣子
Ctrl+B
Type "z"
Sleep 1
Type "agent-bridge ui"
Enter
# blocker 升旗需連續兩輪命中（2s 一輪，最壞未滿 4s）＋framebuffer 穩定
Sleep 9

# 停在畫面上讀：stat header 的計數、兩行列的第二行摘要與 elapsed、
# 浮到最上面那一組的 ⛔ 徽章
Sleep 5

# 移到浮頂那一列。**選取不會自己跟著排序跑**（stable key 跟人不跟位），
# 所以這兩步是必要的，也正好演出那條紀律；DETAIL 這時顯示框在問什麼
Up
Sleep 0.6
Up
Sleep 7

# Enter：直接跳到卡住的那個 pane
Enter
Sleep 6
