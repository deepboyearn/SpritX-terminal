# Tab open/close smoothness — investigation

## Before: findings recorded before this fix

Issue ka main cause native UI animation/lifecycle wiring mein hai.
Terminal text rendering ya theme colors is issue ka primary cause nahi hain.

1. **Close par duplicate slot:** original tab `app.tabs` mein render hota
   rehta hai aur ek extra closing copy strip ke end mein append hoti hai.
   Isliye clicked position par smooth collapse nahi hota.
2. **Animation width initialize nahi hoti:** `ClosingTab.from_width` zero se
   start hoti hai; code mein ise measured width assign nahi hoti. Renderer
   zero dekhkar har frame full scale choose karta hai. End par removal snap hota hai.
3. **Paint aur layout mismatch:** `request_animation_frame()` paint request
   karta hai, lekin slot width Rust render/layout build mein calculate hoti hai.
   Animation ko time ke saath entity notification/layout rebuild bhi chahiye.
4. **Hard-coded width:** collapse 132px assume karta hai, jabki content ke
   hisaab se real tab width badal sakti hai. Gap bhi independently disappear hota hai.
5. **Open animation missing:** add-tab turant full-width slot insert karta hai.
   Existing 210ms motion selected highlight ki hai, new slot ki nahi.
6. **Synchronous lifecycle work:** add-tab terminal/session startup ko UI
   callback se call karta hai. Owner startup mein a readiness wait loop hai.
   Close teardown owner kill/detach request bhi synchronous hai. Existing
   checkout mein persistence writes already background writer ko queued hain;
   unhe is bug ka confirmed filesystem stall cause nahi maana gaya.
   Ye responsiveness risks source se confirmed hain; is investigation mein
   abhi koi numeric frame-rate ya startup-latency measurement claim nahi hai.
7. **Rapid-close index risk:** pending close flush karne ke baad requested
   numeric index shift ho sakta hai. Intended target ko stable tab ID se resolve
   karna chahiye.

Working tree mein pehle se pending changes hain. Is fix mein un unrelated
changes ko reset/revert nahi kiya jayega.

## Fix and after-validation

| Area | Before | After |
| --- | --- | --- |
| New tab | Full-width slot turant insert hota tha | Real tab slot aur uska gap 210ms ease-out mein expand hote hain |
| Close | Original tab + separate copy; end par snap | Usi position par ek real tab collapse hota hai, phir remove hota hai |
| Width | Fixed/zero width assumption | Native layout se har tab ki actual width measure hoti hai |
| Frames | Paint requests se layout width stale reh sakti thi | Motion ke dauran 16ms timer entity notify karke layout rebuild karta hai |
| Rapid clicks | Pending close aur numeric indices conflict kar sakte the | Har transition stable ID par; simultaneous closes independent hain |
| Session lifecycle | New-tab startup aur close teardown UI callback par | Startup aur session disposal background mein; stale startup result closed pane ko reopen nahi karta |
| Early typing | Startup complete hone ka wait UI ko block karta tha | Startup input bounded queue mein rehta hai aur session ready hone par deliver hota hai |
| Accessibility | Motion paths inconsistent the | Reduced motion par immediate open/close; pending startup ko Dead nahi dikhaya jata |

Existing colors, connected rounded-tab design aur approved Composing orb ko
is fix mein redesign nahi kiya gaya.

### Windows native-window measurements

Actual GPUI window test ke latest successful run mein:

- Add-tab handler: **422 microseconds (~0.42ms)**.
- Close-tab handler: **37 microseconds (~0.04ms)**.
- Open slot samples, pixels: **58, 97, 137, 143, 144, 144, 144, 144**.
- Close slot samples, pixels: **121, 66, 10, 2**, phir tab remove hua.

Samples approximately 35ms intervals par actual laid-out bounds se liye gaye.
Ye handler timings is machine/run ke hain; shell readiness duration ya fixed
FPS guarantee nahi hain. Before ke liye numeric benchmark capture nahi hua;
before findings source inspection par based hain.

### Validation

- `cargo check -j 2 --tests`: pass.
- `cargo test -j 2 --lib -- --test-threads=1`: **75 passed**.
  Ismein variable-width transitions, concurrent close IDs aur open ko beech
  mein close karne ke regression cases included hain.
- Native `tab_lifecycle` display test: **1 passed**. Intermediate open/close
  widths, split tab removal, last-tab empty state, reduced motion, startup
  queued command ka actual output, save aur restart verify hue.
  Window/font initialization aur shutdown flushes samet test ko pehle 30s
  watchdog hit hua tha; first app ka watchdog 60s kiya. Dono launches wala
  successful complete test 68.95s mein finish hua. Animation duration 210ms
  hi hai; watchdog badalne se animation assertions relax nahi hue.
- Related integration checks: **20 passed** — CLI activity (2), tab/pane
  state (10), terminal rendering regressions (3), workspace restore (5).
- Native persistent-session/process-control test: **1 passed** (38.02s).
  Background new-tab owner startup, keyboard Ctrl+C, pane-scoped force stop
  aur doosre tab ke session ko preserve karna verify hua.
- `cargo build -j 2`: pass; debug executable updated.

Windows par native runtime verify hua. Linux par isi shared Rust transition
aur lifecycle code ka use hai, lekin is Windows environment mein Linux runtime
test nahi kiya gaya.
