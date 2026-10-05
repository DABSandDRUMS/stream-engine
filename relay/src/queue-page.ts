// Public song-queue page (`/queue`) with optional, authenticated moderator management.
// Public updates use `/queue/ws`; private reads and commands use `/mod/api` only when
// the moderator section is opened. Images come from YouTube and Apple Music.

import { escapeHtml } from "./util";

const CSP =
  "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; img-src https://i.ytimg.com https://*.mzstatic.com; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

const STYLE = `
@property --accent{syntax:"<color>";inherits:true;initial-value:#a78bfa}
:root{color-scheme:dark;--bg:#07070a;--fg:#f5f5f7;--dim:rgba(235,235,245,.62);--faint:rgba(235,235,245,.36);--line:rgba(255,255,255,.09);--glass:rgba(20,20,27,.58);--accent:#a78bfa;--r:30px;
  font-family:"Inter Variable",Inter,"SF Pro Display",ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;-webkit-font-smoothing:antialiased;transition:--accent 1.4s ease}
*{box-sizing:border-box}
html,body{margin:0;background:var(--bg);color:var(--fg)}
body{min-height:100dvh;overflow-x:hidden}
a{color:inherit}
.ambient{position:fixed;inset:0;z-index:0;pointer-events:none;overflow:hidden;
  background:radial-gradient(55% 45% at 15% 0%,color-mix(in oklab,var(--accent) 38%,transparent),transparent 70%),radial-gradient(45% 45% at 100% 85%,color-mix(in oklab,var(--accent) 22%,transparent),transparent 70%)}
.ambient img{position:absolute;inset:-15%;width:130%;height:130%;object-fit:cover;filter:blur(90px) saturate(2.4) brightness(.75);opacity:0;transition:opacity 1.4s ease}
.ambient img.on{opacity:.85}
.ambient::after{content:"";position:absolute;inset:0;background:linear-gradient(180deg,rgba(7,7,10,.15) 0%,rgba(7,7,10,.72) 55%,var(--bg) 100%)}
main{position:relative;z-index:1;max-width:1120px;margin:0 auto;padding:clamp(20px,4vw,52px) clamp(14px,4vw,40px) 36px}
.top{display:flex;align-items:flex-end;justify-content:space-between;gap:16px;flex-wrap:wrap;margin-bottom:clamp(20px,3.2vw,36px)}
.eyebrow{display:inline-flex;align-items:center;gap:8px;margin-bottom:8px;font-size:.74rem;font-weight:700;letter-spacing:.16em;text-transform:uppercase;color:var(--dim);text-decoration:none;transition:color .2s}
.eyebrow::before{content:"";width:18px;height:2px;border-radius:2px;background:var(--accent)}
a.eyebrow:hover{color:var(--fg)}
h1{margin:0;font-size:clamp(2rem,5vw,3.4rem);font-weight:850;letter-spacing:-.045em;line-height:.95}
.chips{display:flex;gap:8px;flex-wrap:wrap}
.chip{display:inline-flex;align-items:center;gap:8px;height:34px;padding:0 15px;border-radius:999px;font-size:.82rem;font-weight:650;color:var(--dim);background:rgba(255,255,255,.06);border:1px solid var(--line);backdrop-filter:blur(18px);-webkit-backdrop-filter:blur(18px)}
.chip[hidden]{display:none}
.chip.open{color:#bff5cf}.chip.closed{color:#ffc0c0}
.chip .dot{width:8px;height:8px;border-radius:50%;background:var(--faint)}
.chip.live{color:var(--fg)}
.chip.live .dot{background:#ff4559;animation:pulse 2s ease-out infinite}
.chip.open .dot{background:#4ade80}.chip.closed .dot{background:#f87171}
.card{background:var(--glass);border:1px solid var(--line);border-radius:var(--r);backdrop-filter:blur(30px) saturate(160%);-webkit-backdrop-filter:blur(30px) saturate(160%);box-shadow:0 40px 90px -40px rgba(0,0,0,.8),inset 0 1px 0 rgba(255,255,255,.07)}
.stage{display:grid;grid-template-columns:clamp(220px,32vw,360px) minmax(0,1fr);gap:clamp(22px,3.6vw,48px);align-items:center;padding:clamp(18px,2.6vw,30px);margin-bottom:clamp(16px,2.4vw,28px)}
.stage[hidden],.idle[hidden]{display:none}
.cover{position:relative;aspect-ratio:1;border-radius:22px;overflow:hidden;background:linear-gradient(140deg,color-mix(in oklab,var(--accent) 55%,#15151b),#0d0d12 75%);
  box-shadow:0 46px 90px -34px color-mix(in oklab,var(--accent) 70%,transparent),0 14px 34px -14px rgba(0,0,0,.75)}
.cover::after{content:"";position:absolute;inset:0;border-radius:inherit;box-shadow:inset 0 0 0 1px rgba(255,255,255,.1);pointer-events:none}
.cover img{position:absolute;inset:0;width:100%;height:100%;object-fit:cover;opacity:0;transition:opacity .7s ease}
.cover img.on{opacity:1}
.cover img.yt{transform:scale(1.34)}
.cover .note{position:absolute;inset:0;display:grid;place-items:center;font-size:clamp(3rem,8vw,5.5rem);color:rgba(255,255,255,.18)}
.info{min-width:0;display:flex;flex-direction:column}
.kicker{display:flex;align-items:center;gap:10px;font-size:.76rem;font-weight:750;letter-spacing:.16em;text-transform:uppercase;color:var(--accent)}
.eq{display:inline-flex;align-items:flex-end;gap:3px;height:14px}
.eq i{width:3px;height:100%;border-radius:3px;background:currentColor;transform-origin:bottom;transform:scaleY(.3);animation:eq 1.1s ease-in-out infinite;animation-play-state:paused}
.eq i:nth-child(2){animation-delay:-.45s}.eq i:nth-child(3){animation-delay:-.8s}.eq i:nth-child(4){animation-delay:-.2s}
.stage.playing .eq i{animation-play-state:running}
.song{margin:14px 0 0;font-size:clamp(1.9rem,4.6vw,3.5rem);font-weight:850;letter-spacing:-.04em;line-height:1.02;text-wrap:balance;overflow-wrap:anywhere}
.artist{margin:10px 0 0;font-size:clamp(1.05rem,2vw,1.35rem);font-weight:600;color:color-mix(in oklab,var(--accent) 45%,white);overflow-wrap:anywhere}
.album{margin:4px 0 0;font-size:.92rem;color:var(--faint)}
.album:empty{display:none}
.requester{display:inline-flex;align-items:center;gap:10px;align-self:flex-start;margin-top:22px;padding:6px 14px 6px 6px;border-radius:999px;background:rgba(255,255,255,.06);border:1px solid var(--line);font-size:.88rem;color:var(--dim)}
.requester b{color:var(--fg);font-weight:650}
.avatar{display:grid;place-items:center;width:28px;height:28px;border-radius:50%;font-size:.78rem;font-weight:800;color:#fff;background:var(--accent);flex:none}
.progress{margin-top:clamp(22px,3vw,34px)}
.rail{height:6px;border-radius:99px;background:rgba(255,255,255,.12);overflow:hidden}
.rail i{display:block;height:100%;width:0;border-radius:inherit;background:linear-gradient(90deg,color-mix(in oklab,var(--accent) 55%,white),var(--accent));box-shadow:0 0 20px color-mix(in oklab,var(--accent) 80%,transparent);transition:width .5s linear}
.time{display:flex;justify-content:space-between;margin-top:10px;font-size:.8rem;font-weight:600;color:var(--dim);font-variant-numeric:tabular-nums}
.idle{display:flex;align-items:center;gap:clamp(20px,4vw,44px);padding:clamp(22px,3vw,36px);margin-bottom:clamp(16px,2.4vw,28px)}
.disc{flex:none;width:clamp(120px,22vw,190px);aspect-ratio:1;border-radius:50%;
  background:radial-gradient(circle,var(--accent) 0 15%,#0a0a0e 15.5% 17%,transparent 17.5%),repeating-radial-gradient(circle,#18181d 0 1.5px,#0d0d11 1.5px 3.5px);
  box-shadow:0 30px 60px -20px rgba(0,0,0,.8),inset 0 0 0 1px rgba(255,255,255,.06);animation:spin 7s linear infinite}
.idle h2{margin:0;font-size:clamp(1.6rem,3.6vw,2.4rem);font-weight:850;letter-spacing:-.035em}
.idle p{margin:10px 0 0;color:var(--dim);font-size:1.02rem;line-height:1.5}
.upnext{padding:clamp(16px,2.4vw,26px)}
.up-head{display:flex;align-items:baseline;justify-content:space-between;gap:12px;padding:4px 6px 14px}
.up-head h3{margin:0;font-size:1.25rem;font-weight:800;letter-spacing:-.02em}
.up-meta{font-size:.85rem;color:var(--faint);font-variant-numeric:tabular-nums}
.list{list-style:none;margin:0;padding:0;display:flex;flex-direction:column;gap:4px}
.list li{display:grid;grid-template-columns:28px 56px minmax(0,1fr) auto;gap:14px;align-items:center;padding:8px 12px 8px 6px;border-radius:18px;transition:background .25s,opacity .5s,transform .5s cubic-bezier(.2,.8,.2,1)}
@starting-style{.list li{opacity:0;transform:translateY(10px)}}
.list li:hover{background:rgba(255,255,255,.05)}
.list li.first{background:color-mix(in oklab,var(--accent) 13%,transparent)}
.list .n{text-align:right;font-size:.9rem;font-weight:700;color:var(--faint);font-variant-numeric:tabular-nums}
.thumb{position:relative;width:56px;height:56px;border-radius:12px;overflow:hidden;background:linear-gradient(140deg,#25252d,#121217)}
.thumb img{width:100%;height:100%;object-fit:cover;display:block}
.t{min-width:0}
.t .s{font-weight:700;font-size:1rem;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.t .a{margin-top:3px;font-size:.84rem;color:var(--dim);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.tag{display:inline-block;margin-right:8px;padding:1px 8px;border-radius:999px;font-size:.66rem;font-weight:800;letter-spacing:.1em;text-transform:uppercase;vertical-align:2px;background:var(--accent);color:#0a0a0e}
.r{text-align:right;font-variant-numeric:tabular-nums}
.r .d{font-size:.9rem;font-weight:650}
.r .e{margin-top:3px;font-size:.76rem;color:var(--faint);white-space:nowrap}
.empty{padding:26px 12px;text-align:center;color:var(--dim);border:1px dashed var(--line);border-radius:18px}
.list li.empty{display:block;padding:22px 12px}
.cta{display:flex;align-items:center;justify-content:space-between;gap:14px;flex-wrap:wrap;margin-top:14px;padding:16px 18px;border-radius:20px;background:rgba(255,255,255,.045);border:1px solid var(--line)}
.cta-text{display:flex;align-items:center;gap:12px;flex-wrap:wrap;font-weight:650}
.cta code{font-family:ui-monospace,"SF Mono","JetBrains Mono",Menlo,monospace;font-size:.88rem;padding:6px 11px;border-radius:10px;background:rgba(0,0,0,.35);border:1px solid var(--line);color:color-mix(in oklab,var(--accent) 40%,white)}
.cta.closed code{display:none}
.btn{display:inline-flex;align-items:center;gap:8px;height:38px;padding:0 16px;border-radius:999px;font-size:.88rem;font-weight:750;text-decoration:none;color:#0a0a0e;background:var(--fg);transition:transform .2s,box-shadow .2s}
.btn:hover{transform:translateY(-1px);box-shadow:0 10px 26px -8px color-mix(in oklab,var(--accent) 80%,transparent)}
footer{margin-top:28px;text-align:center;font-size:.78rem;color:var(--faint);letter-spacing:.04em}
.moderator{margin-top:18px;padding:16px 20px}
.moderator summary{cursor:pointer;font-size:.86rem;font-weight:650;color:var(--dim)}
.moderator summary:hover{color:var(--fg)}
.moderator [hidden]{display:none!important}
.mod-body{padding-top:14px;font-size:.9rem}
.mod-body p{margin:0 0 12px;line-height:1.5;color:var(--dim);overflow-wrap:anywhere}
.mod-toolbar,.mod-actions{display:flex;align-items:center;gap:8px;flex-wrap:wrap}
.mod-toolbar{justify-content:space-between;margin-bottom:12px}
.mod-toolbar a{font-size:.85rem}
.mod-button{font:inherit;font-size:.82rem;min-height:38px;padding:7px 12px;color:var(--fg);background:rgba(255,255,255,.07);border:1px solid var(--line);border-radius:10px;cursor:pointer}
.mod-button:hover:not(:disabled){background:color-mix(in oklab,var(--accent) 18%,transparent)}
.mod-button:disabled{opacity:.45;cursor:not-allowed}
.mod-button:focus-visible,.moderator summary:focus-visible,.mod-input:focus-visible,.mod-login:focus-visible{outline:2px solid var(--accent);outline-offset:3px}
.mod-login{display:inline-block;margin-bottom:12px}
.mod-form{display:flex;align-items:end;gap:10px;flex-wrap:wrap;margin:16px 0}
.mod-field{flex:1;min-width:min(100%,230px);display:grid;gap:6px}
.mod-input{width:100%;min-height:42px;padding:10px 12px;font:inherit;color:var(--fg);background:rgba(0,0,0,.18);border:1px solid var(--line);border-radius:10px}
.mod-section h3{font-size:1rem;margin:20px 0 10px}
.mod-list{list-style:none;padding:0;margin:0;display:grid;gap:8px}
.mod-row{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:12px;padding:12px;border:1px solid var(--line);border-radius:12px}
.mod-title{font-weight:700;overflow-wrap:anywhere}
.mod-sub{margin-top:4px;font-size:.8rem;color:var(--dim);overflow-wrap:anywhere}
.mod-drag{font-size:.76rem;color:var(--dim);cursor:grab;padding:9px 5px}
.mod-row.dragging{opacity:.5}
.mod-row.drop-before{border-top:3px solid var(--accent)}
.mod-row.drop-after{border-bottom:3px solid var(--accent)}
.mod-message{min-height:1.4em}
.mod-message.error{color:#ffb7b7}
@media(max-width:760px){.moderator{padding:14px}.mod-row{grid-template-columns:1fr}.mod-actions{justify-content:flex-end}.mod-form>.mod-button{width:100%}}
@keyframes pulse{0%{box-shadow:0 0 0 0 rgba(255,69,89,.55)}100%{box-shadow:0 0 0 10px rgba(255,69,89,0)}}
@keyframes eq{0%,100%{transform:scaleY(.3)}50%{transform:scaleY(1)}}
@keyframes spin{to{transform:rotate(360deg)}}
@media (max-width:760px){
  .stage{grid-template-columns:1fr;gap:22px}
  .cover{width:min(100%,420px);justify-self:center}
  .idle{flex-direction:column;text-align:center}
  .list li{grid-template-columns:22px 48px minmax(0,1fr) auto;gap:11px;padding-right:8px}
  .thumb{width:48px;height:48px;border-radius:10px}
  .t .s{display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;white-space:normal;line-height:1.25}
  .cta{justify-content:center;text-align:center}.cta-text{justify-content:center}
}
.window-title{display:none}
/* Windows 3.1: one desktop and beveled, non-interactive window chrome. The
   modern rules above stay intact so a live theme change only changes styling. */
:root[data-theme="win31"]{color-scheme:light;--bg:#008080;--fg:#000;--dim:#404040;--faint:#505050;--line:#808080;--glass:#c0c0c0;--r:0;
  font-family:"MS Sans Serif","Microsoft Sans Serif",Tahoma,Arial,sans-serif;font-size:14px;-webkit-font-smoothing:auto;transition:none}
:root[data-theme="win31"] *, :root[data-theme="win31"] *::before, :root[data-theme="win31"] *::after{animation:none;transition:none}
:root[data-theme="win31"] .ambient{display:none}
:root[data-theme="win31"] main{max-width:960px;margin:clamp(12px,4vw,42px) auto;padding:8px;background:#c0c0c0;border:1px solid #000;
  box-shadow:inset 1px 1px #fff,inset -1px -1px #808080,4px 4px 0 rgba(0,0,0,.3)}
:root[data-theme="win31"] .top{align-items:center;gap:12px;margin:0 0 12px;padding:8px 10px;background:#000080;color:#fff}
:root[data-theme="win31"] .top>div:first-child{min-width:0;flex:1}
:root[data-theme="win31"] .eyebrow{margin-bottom:4px;color:#fff;font-size:12px;font-weight:400;letter-spacing:0;text-transform:none;overflow-wrap:anywhere}
:root[data-theme="win31"] .eyebrow::before{width:8px;height:8px;border-radius:0;background:#fff}
:root[data-theme="win31"] a.eyebrow:hover{text-decoration:underline}
:root[data-theme="win31"] h1{font-size:clamp(20px,3vw,28px);font-weight:700;letter-spacing:0;line-height:1.2;overflow-wrap:anywhere}
:root[data-theme="win31"] .chips{gap:6px}
:root[data-theme="win31"] .chip{height:auto;min-height:26px;padding:4px 8px;border-radius:0;background:#c0c0c0;color:#000;font-size:12px;font-weight:400;
  border:1px solid #808080;box-shadow:inset 1px 1px #fff,inset -1px -1px #a0a0a0;backdrop-filter:none;-webkit-backdrop-filter:none}
:root[data-theme="win31"] .chip .dot{width:7px;height:7px;border-radius:0;background:#606060}
:root[data-theme="win31"] .chip.live .dot,:root[data-theme="win31"] .chip.open .dot{background:#008000}
:root[data-theme="win31"] .chip.closed .dot{background:#800000}
:root[data-theme="win31"] .card{border:1px solid #808080;border-radius:0;background:#c0c0c0;
  box-shadow:inset 1px 1px #fff,inset -1px -1px #404040;backdrop-filter:none;-webkit-backdrop-filter:none}
:root[data-theme="win31"] .window-title{display:block;grid-column:1/-1;align-self:stretch;margin:-4px -4px 0;padding:4px 8px;
  background:#000080;color:#fff;font-size:13px;font-weight:700;line-height:1.3}
:root[data-theme="win31"] .stage{grid-template-columns:clamp(160px,26vw,240px) minmax(0,1fr);gap:14px 20px;padding:8px;margin-bottom:12px;align-items:center}
:root[data-theme="win31"] .cover{border:2px solid;border-color:#808080 #fff #fff #808080;border-radius:0;background:#fff;box-shadow:none}
:root[data-theme="win31"] .cover::after{box-shadow:inset 1px 1px #000}
:root[data-theme="win31"] .cover .note{color:#000080;font-size:64px}
:root[data-theme="win31"] .info{padding:4px 8px 8px 0}
:root[data-theme="win31"] .kicker{gap:8px;color:#000080;font-size:12px;font-weight:700;letter-spacing:0;text-transform:none}
:root[data-theme="win31"] .eq i{width:3px;border-radius:0;transform:scaleY(.35)}
:root[data-theme="win31"] .stage.playing .eq i:nth-child(2){transform:scaleY(.75)}
:root[data-theme="win31"] .stage.playing .eq i:nth-child(3){transform:scaleY(1)}
:root[data-theme="win31"] .stage.playing .eq i:nth-child(4){transform:scaleY(.55)}
:root[data-theme="win31"] .song{margin-top:10px;font-size:clamp(22px,3.5vw,34px);font-weight:700;letter-spacing:0;line-height:1.15;text-wrap:wrap}
:root[data-theme="win31"] .artist{margin-top:6px;color:#000;font-size:16px;font-weight:400}
:root[data-theme="win31"] .album{font-size:12px;color:#404040;overflow-wrap:anywhere}
:root[data-theme="win31"] .requester{max-width:100%;margin-top:14px;padding:4px 8px 4px 4px;gap:6px;border-radius:0;background:#c0c0c0;border:1px solid;
  border-color:#808080 #fff #fff #808080;font-size:12px;color:#404040}
:root[data-theme="win31"] .requester>span:last-child{min-width:0;overflow-wrap:anywhere}
:root[data-theme="win31"] .requester b{font-weight:700;color:#000}
:root[data-theme="win31"] .avatar{width:24px;height:24px;border-radius:0;background:#000080!important;font-size:12px}
:root[data-theme="win31"] .progress{margin-top:16px}
:root[data-theme="win31"] .rail{height:18px;padding:2px;border:2px solid;border-color:#808080 #fff #fff #808080;border-radius:0;background:#fff}
:root[data-theme="win31"] .rail i{border-radius:0;background:repeating-linear-gradient(90deg,#000080 0 8px,transparent 8px 10px);box-shadow:none}
:root[data-theme="win31"] .time{margin-top:5px;font-family:"Courier New",monospace;font-size:12px;font-weight:400;color:#000}
:root[data-theme="win31"] .idle{display:grid;grid-template-columns:80px minmax(0,1fr);gap:14px 18px;padding:8px;margin-bottom:12px;text-align:left}
:root[data-theme="win31"] .idle[hidden]{display:none}
:root[data-theme="win31"] .disc{display:grid;place-items:center;width:72px;margin:4px;border:2px solid;border-color:#808080 #fff #fff #808080;
  border-radius:0;background:#fff;box-shadow:inset 1px 1px #000}
:root[data-theme="win31"] .disc::after{content:"♫";font-family:Arial,sans-serif;font-size:44px;color:#000080}
:root[data-theme="win31"] .idle>div:last-child{min-width:0;padding:8px 8px 12px 0}
:root[data-theme="win31"] .idle h2{font-size:20px;font-weight:700;letter-spacing:0;overflow-wrap:anywhere}
:root[data-theme="win31"] .idle p{margin-top:6px;font-size:13px;line-height:1.5;overflow-wrap:anywhere}
:root[data-theme="win31"] .upnext{padding:8px}
:root[data-theme="win31"] .up-head{align-items:center;margin:-4px -4px 8px;padding:5px 8px;background:#000080;color:#fff;flex-wrap:wrap}
:root[data-theme="win31"] .up-head h3{font-size:13px;font-weight:700;letter-spacing:0}
:root[data-theme="win31"] .up-meta{font-size:12px;color:#fff}
:root[data-theme="win31"] .list{gap:0;min-height:88px;padding:2px;border:2px solid;border-color:#808080 #fff #fff #808080;background:#fff;box-shadow:inset 1px 1px #000}
:root[data-theme="win31"] .list li{grid-template-columns:24px 44px minmax(0,1fr) 76px;gap:10px;padding:8px;border-radius:0;transform:none;opacity:1;border-bottom:1px solid #dfdfdf}
:root[data-theme="win31"] .list li:last-child{border-bottom:0}
:root[data-theme="win31"] .list li:hover{background:#ededed}
:root[data-theme="win31"] .list li.first,:root[data-theme="win31"] .list li.first:hover{background:#000080;color:#fff}
:root[data-theme="win31"] .list .n{color:#404040;font-size:12px;font-weight:400}
:root[data-theme="win31"] .thumb{width:44px;height:44px;border:1px solid #808080;border-radius:0;background:#c0c0c0}
:root[data-theme="win31"] .t .s{font-size:14px;font-weight:700}
:root[data-theme="win31"] .t .a{font-size:12px;color:#404040}
:root[data-theme="win31"] .tag{margin-right:6px;padding:1px 4px;border:1px solid #808080;border-radius:0;background:#c0c0c0;color:#000;
  font-size:10px;font-weight:400;letter-spacing:0;text-transform:none;vertical-align:1px}
:root[data-theme="win31"] .r{min-width:0}
:root[data-theme="win31"] .r .d{font-family:"Courier New",monospace;font-size:12px;font-weight:400}
:root[data-theme="win31"] .r .e{font-size:11px;color:#404040;white-space:normal}
:root[data-theme="win31"] .first .n,:root[data-theme="win31"] .first .a,:root[data-theme="win31"] .first .e{color:#fff}
:root[data-theme="win31"] .list li.empty{display:block;padding:24px 12px;border:0;border-radius:0;background:#fff;color:#404040;font-size:13px;line-height:1.5}
:root[data-theme="win31"] .cta{margin-top:10px;padding:10px;gap:10px;border:1px solid;border-color:#808080 #fff #fff #808080;border-radius:0;background:#c0c0c0}
:root[data-theme="win31"] .cta-text{min-width:0;max-width:100%;gap:8px;font-size:13px;font-weight:400}
:root[data-theme="win31"] .cta code{max-width:100%;padding:5px 7px;border:1px solid;border-color:#808080 #fff #fff #808080;border-radius:0;
  background:#fff;color:#000;font-family:"Courier New",monospace;font-size:12px;white-space:normal;overflow-wrap:anywhere}
:root[data-theme="win31"] .btn{height:auto;min-height:34px;padding:7px 18px;justify-content:center;border:2px solid;border-color:#fff #404040 #404040 #fff;
  border-radius:0;background:#c0c0c0;color:#000;font-size:13px;font-weight:400;box-shadow:1px 1px 0 #000}
:root[data-theme="win31"] .btn:hover{transform:none;background:#d0d0d0;box-shadow:1px 1px 0 #000}
:root[data-theme="win31"] .btn:active{border-color:#404040 #fff #fff #404040;box-shadow:none}
:root[data-theme="win31"] .btn:focus-visible{outline:1px dotted #000;outline-offset:-6px}
:root[data-theme="win31"] a.eyebrow:focus-visible{outline:1px dotted #fff;outline-offset:3px}
:root[data-theme="win31"] footer{margin-top:10px;padding:8px;border:1px solid;border-color:#808080 #fff #fff #808080;text-align:left;font-size:11px;color:#404040;letter-spacing:0}
:root[data-theme="win31"] .moderator{margin-top:10px;padding:10px}
:root[data-theme="win31"] .moderator summary{font-size:12px;color:#404040;font-weight:400}
:root[data-theme="win31"] .mod-body{font-size:13px}
:root[data-theme="win31"] .mod-button{border:2px solid;border-color:#fff #404040 #404040 #fff;border-radius:0;background:#c0c0c0;color:#000;box-shadow:1px 1px 0 #000;font-size:12px}
:root[data-theme="win31"] .mod-button:hover:not(:disabled){background:#d0d0d0}
:root[data-theme="win31"] .mod-button:active:not(:disabled){border-color:#404040 #fff #fff #404040;box-shadow:none}
:root[data-theme="win31"] .mod-input,:root[data-theme="win31"] .mod-row{border:2px solid;border-color:#808080 #fff #fff #808080;border-radius:0;background:#fff;color:#000}
:root[data-theme="win31"] .mod-input{font-size:13px}
:root[data-theme="win31"] .mod-sub,:root[data-theme="win31"] .mod-drag{color:#404040}
:root[data-theme="win31"] .mod-section h3{font-size:14px}
:root[data-theme="win31"] .mod-message.error{color:#800000}
:root[data-theme="win31"] .mod-row.drop-before{border-top:3px solid #000080}
:root[data-theme="win31"] .mod-row.drop-after{border-bottom:3px solid #000080}
:root[data-theme="win31"] .mod-button:focus-visible,:root[data-theme="win31"] .moderator summary:focus-visible,:root[data-theme="win31"] .mod-input:focus-visible,:root[data-theme="win31"] .mod-login:focus-visible{outline:1px dotted #000;outline-offset:2px}
@media (max-width:980px){:root[data-theme="win31"] main{margin-left:12px;margin-right:12px}}
@media (max-width:600px){
  :root[data-theme="win31"] main{margin:10px;padding:6px}
  :root[data-theme="win31"] .top{padding:8px;gap:10px}
  :root[data-theme="win31"] .chips{width:100%}
  :root[data-theme="win31"] .stage{grid-template-columns:1fr;gap:12px}
  :root[data-theme="win31"] .cover{width:min(100%,240px)}
  :root[data-theme="win31"] .info{padding:0 4px 4px}
  :root[data-theme="win31"] .idle{grid-template-columns:54px minmax(0,1fr);gap:10px}
  :root[data-theme="win31"] .disc{width:48px;margin:2px}
  :root[data-theme="win31"] .disc::after{font-size:30px}
  :root[data-theme="win31"] .idle h2{font-size:17px}
  :root[data-theme="win31"] .list li{grid-template-columns:18px 36px minmax(0,1fr) 60px;gap:6px;padding:7px 4px}
  :root[data-theme="win31"] .thumb{width:36px;height:36px}
  :root[data-theme="win31"] .t .s{display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;white-space:normal;font-size:13px;line-height:1.3;overflow-wrap:anywhere}
  :root[data-theme="win31"] .t .a{font-size:11px}
  :root[data-theme="win31"] .cta{justify-content:center;text-align:center}
}
@media (prefers-reduced-motion:reduce){*,*::before,*::after{animation:none!important;transition:none!important}}
`;

const SCRIPT = `"use strict";
(() => {
  const $ = (id) => document.getElementById(id);
  const reduced = matchMedia("(prefers-reduced-motion: reduce)").matches;
  const baseTitle = document.title;
  const defaultAccent = getComputedStyle(document.documentElement).getPropertyValue("--accent").trim() || "#a78bfa";
  const YT = /^[A-Za-z0-9_-]{11}$/;
  let state = { online: false, snapshot: null };
  let theme = "win31";
  let skew = 0; // relay clock − local clock (ms)
  let shownVideo = null;
  let listKey = "";
  let etas = [];
  const fmt = (s) => {
    s = Math.max(0, Math.floor(s || 0));
    const h = Math.floor(s / 3600), m = Math.floor(s / 60) % 60, x = String(s % 60).padStart(2, "0");
    return h ? h + ":" + String(m).padStart(2, "0") + ":" + x : m + ":" + x;
  };
  const songOf = (e) => e.song || e.title || "";
  const artistOf = (e) => e.artist || e.channel || "";
  const position = () => {
    const n = state.snapshot && state.snapshot.now;
    if (!n) return 0;
    let p = n.position || 0;
    if (n.playing && state.online && n.at) p += (Date.now() + skew - n.at) / 1000;
    return Math.min(Math.max(0, p), n.duration || p);
  };
  // Image candidates: Apple artwork first (CORS-enabled, used for the accent color), then YouTube.
  function sources(e, big) {
    const out = [];
    if (typeof e.art === "string" && e.art.startsWith("https://")) out.push({ url: e.art, art: true });
    if (YT.test(e.video || "")) out.push({ url: "https://i.ytimg.com/vi/" + e.video + (big ? "/hqdefault.jpg" : "/mqdefault.jpg"), art: false });
    return out;
  }
  function load(img, list, onload) {
    const next = list[0];
    img.onload = img.onerror = null;
    img.classList.remove("on");
    if (!next) { img.removeAttribute("src"); return; }
    if (next.art) img.crossOrigin = "anonymous"; else img.removeAttribute("crossorigin");
    img.onload = () => { img.classList.add("on"); if (onload) onload(img, next); };
    img.onerror = () => load(img, list.slice(1), onload);
    img.src = next.url;
  }
  function accentFrom(img) {
    try {
      const canvas = document.createElement("canvas");
      canvas.width = canvas.height = 32;
      const g = canvas.getContext("2d", { willReadFrequently: true });
      g.drawImage(img, 0, 0, 32, 32);
      const d = g.getImageData(0, 0, 32, 32).data;
      // Dominant vivid hue: 12 hue bins weighted by saturation² × brightness; greyscale art
      // keeps the default accent. Output is normalized for contrast on the dark page.
      const bins = Array.from({ length: 12 }, () => ({ w: 0, r: 0, g: 0, b: 0 }));
      for (let i = 0; i < d.length; i += 4) {
        const R = d[i], G = d[i + 1], B = d[i + 2];
        const mx = Math.max(R, G, B), mn = Math.min(R, G, B), c = mx - mn;
        if (!mx || !c) continue;
        const h = mx === R ? ((G - B) / c + 6) % 6 : mx === G ? (B - R) / c + 2 : (R - G) / c + 4;
        const wt = (c / mx) ** 2 * (mx / 255);
        const bin = bins[Math.floor(h * 2) % 12];
        bin.w += wt; bin.r += R * wt; bin.g += G * wt; bin.b += B * wt;
      }
      const best = bins.reduce((a, b) => (b.w > a.w ? b : a));
      if (best.w < 1.5) return null;
      const R = best.r / best.w / 255, G = best.g / best.w / 255, B = best.b / best.w / 255;
      const mx = Math.max(R, G, B), c = mx - Math.min(R, G, B);
      const h = 60 * (mx === R ? ((G - B) / c + 6) % 6 : mx === G ? (B - R) / c + 2 : (R - G) / c + 4);
      const s = Math.min(92, Math.max(58, (c / mx) * 100));
      return "hsl(" + Math.round(h) + " " + Math.round(s) + "% 64%)";
    } catch { return null; }
  }
  function hue(name) {
    let h = 0;
    for (const ch of name) h = (h * 31 + ch.charCodeAt(0)) % 360;
    return h;
  }
  function showNow(n) {
    const cover = $("cover"), art = $("art");
    $("song").textContent = songOf(n);
    $("artist").textContent = artistOf(n);
    $("album").textContent = n.album || "";
    $("req-name").textContent = n.user || "chat";
    $("avatar").textContent = (n.user || "?").slice(0, 1).toUpperCase();
    $("avatar").style.background = "hsl(" + hue(n.user || "") + " 70% 52%)";
    $("dur").textContent = fmt(n.duration);
    const key = n.video + "|" + (n.art || "");
    if (shownVideo === key) return;
    const changed = shownVideo !== null && shownVideo.split("|")[0] !== n.video;
    shownVideo = key;
    const list = sources(n, true);
    load(art, list, (img, src) => {
      img.classList.toggle("yt", !src.art);
      document.documentElement.style.setProperty("--accent", (src.art && accentFrom(img)) || defaultAccent);
    });
    load($("ambient"), list);
    if (changed && !reduced && theme === "modern") {
      cover.animate([{ opacity: 0, transform: "scale(.94)", filter: "blur(14px)" }, { opacity: 1, transform: "none", filter: "none" }], { duration: 750, easing: "cubic-bezier(.2,.8,.2,1)" });
    }
  }
  function renderList(s) {
    const up = Array.isArray(s.upcoming) ? s.upcoming : [];
    const key = JSON.stringify(up.map((e) => [e.video, e.pos, e.art, e.user]));
    const total = up.reduce((t, e) => t + (e.duration || 0), 0);
    const count = s.length || up.length;
    $("up-meta").textContent = count ? count + (count === 1 ? " song" : " songs") + " · " + Math.max(1, Math.round(total / 60)) + " min" : "";
    if (key === listKey) return;
    listKey = key;
    etas = [];
    const ol = $("upcoming");
    ol.replaceChildren();
    up.forEach((e, i) => {
      const li = document.createElement("li");
      if (i === 0) li.className = "first";
      const n = document.createElement("span"); n.className = "n"; n.textContent = e.pos || i + 1;
      const th = document.createElement("div"); th.className = "thumb";
      const img = document.createElement("img"); img.alt = ""; img.loading = "lazy"; img.decoding = "async";
      th.append(img);
      load(img, sources(e, false));
      const t = document.createElement("div"); t.className = "t";
      const so = document.createElement("div"); so.className = "s";
      if (i === 0) { const tag = document.createElement("span"); tag.className = "tag"; tag.textContent = "Next"; so.append(tag); }
      so.append(songOf(e));
      const a = document.createElement("div"); a.className = "a";
      a.textContent = artistOf(e) + (e.user ? " · requested by " + e.user : "");
      t.append(so, a);
      const r = document.createElement("div"); r.className = "r";
      const d = document.createElement("div"); d.className = "d"; d.textContent = fmt(e.duration);
      const eta = document.createElement("div"); eta.className = "e";
      r.append(d, eta);
      etas.push({ el: eta, duration: e.duration || 0 });
      li.append(n, th, t, r);
      ol.append(li);
    });
    if (!up.length) {
      const li = document.createElement("li"); li.className = "empty";
      li.textContent = s.open === false ? "The queue is empty." : "The queue is empty — your song could be next.";
      ol.append(li);
    }
    const more = count - up.length;
    if (more > 0) {
      const li = document.createElement("li"); li.className = "empty";
      li.textContent = "+ " + more + " more";
      ol.append(li);
    }
  }
  function render() {
    const s = state.snapshot || {};
    // Status-only updates (or a missing snapshot while offline) keep the last
    // theme. Legacy snapshots without a theme use the Windows 3.1 default.
    if (state.snapshot) theme = s.theme === "modern" ? "modern" : "win31";
    document.documentElement.dataset.theme = theme;
    document.querySelector('meta[name="color-scheme"]').content = theme === "modern" ? "dark" : "light";
    document.querySelector('meta[name="theme-color"]').content = theme === "modern" ? "#07070a" : "#008080";
    const st = $("status");
    st.className = "chip status" + (state.online ? " live" : "");
    $("status-text").textContent = state.online ? "Live" : "Offline";
    const req = $("requests");
    req.hidden = !state.online || typeof s.open !== "boolean";
    req.className = "chip " + (s.open ? "open" : "closed");
    $("requests-text").textContent = s.open ? "Requests open" : "Requests closed";
    const cta = $("cta");
    cta.classList.toggle("closed", s.open === false);
    $("cta-label").textContent = s.open === false ? "Requests are closed right now" : "Request a song in chat";
    const n = state.online ? s.now : null;
    $("stage").hidden = !n;
    $("idle").hidden = !!n;
    if (n) {
      showNow(n);
      $("stage").classList.toggle("playing", !!n.playing && !s.paused);
      $("kicker").textContent = s.paused ? "Paused" : n.playing ? "Now playing" : "Starting";
      document.title = songOf(n) + " · " + artistOf(n) + " — " + baseTitle;
    } else {
      shownVideo = null;
      $("ambient").classList.remove("on");
      document.documentElement.style.setProperty("--accent", defaultAccent);
      $("idle-title").textContent = state.online ? "Nothing's playing" : "The stream is offline";
      $("idle-text").textContent = state.online
        ? (s.open === false ? "Requests are closed for now." : "Type a request in chat and it lands right here.")
        : "The queue comes back when the stream does.";
      document.title = baseTitle;
    }
    renderList(s);
    tick();
  }
  function tick() {
    const s = state.snapshot || {};
    const n = state.online ? s.now : null;
    let wait = 0;
    if (n) {
      const p = position();
      wait = Math.max(0, (n.duration || 0) - p);
      $("pos").textContent = fmt(p);
      const pct = n.duration ? Math.min(100, (p / n.duration) * 100) : 0;
      $("bar").style.width = pct + "%";
      $("progress").setAttribute("aria-valuenow", String(Math.round(pct)));
    }
    const live = state.online && !s.paused;
    for (const e of etas) {
      e.el.textContent = !live ? "" : wait < 60 ? "up soon" : "in ~" + Math.round(wait / 60) + " min";
      wait += e.duration;
    }
  }
  // No private request is made until the optional section is opened.
  const modDetails = $("moderator");
  let modQueue = null, modActions = [], modLogin = "";
  let modRead = null, modController = null, modEpoch = 0;
  let modBusy = false, modFresh = false, modBlocked = false;
  let modTimer = null, modLiveTimer = null, modListKey = "", modDrag = null;
  let socketConnected = false;
  const queueIds = (q) => JSON.stringify((q.upcoming || []).map((e) => e.id));
  const modAllowed = (action) => modActions.some((pattern) => {
    // Same dot-segment * / ** matching as the existing moderator console.
    function segment(p, s) {
      let i = 0, j = 0, star = -1, retry = 0;
      while (j < s.length) {
        if (p[i] === s[j]) { i++; j++; }
        else if (p[i] === "*") { star = i++; retry = j; }
        else if (star >= 0) { i = star + 1; j = ++retry; }
        else return false;
      }
      while (p[i] === "*") i++;
      return i === p.length;
    }
    const p = pattern.split("."), a = action.split(".");
    function match(i, j) {
      if (i === p.length) return j === a.length;
      if (p[i] === "**") {
        for (let k = j; k <= a.length; k++) if (match(i + 1, k)) return true;
        return false;
      }
      return j < a.length && segment(p[i], a[j]) && match(i + 1, j + 1);
    }
    return match(0, 0);
  });
  function modElement(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined) e.textContent = String(text);
    return e;
  }
  function modMessage(text, error) {
    $("mod-message").textContent = text;
    $("mod-message").classList.toggle("error", !!error);
  }
  function modAccessError(error) {
    if (error.status !== 401 && error.status !== 403) return false;
    modQueue = null; modActions = []; modLogin = ""; modFresh = false; modBlocked = true;
    modDrag = null;
    $("mod-panel").hidden = true;
    $("mod-login").hidden = error.status !== 401;
    $("mod-retry").hidden = error.status === 401;
    $("mod-status").textContent = error.status === 401
      ? "Signed out. Sign in with Twitch only if you want to manage requests."
      : "Moderator access denied: " + error.message;
    return true;
  }
  async function modPost(body, signal) {
    const response = await fetch("/mod/api", {
      method: "POST", credentials: "same-origin", signal,
      headers: { "Content-Type": "application/json", "X-SE-Mod": "1" },
      body: JSON.stringify(body)
    });
    let data;
    try { data = await response.json(); } catch { data = null; }
    if (!response.ok || !data || data.ok !== true) {
      const error = new Error(data && typeof data.error === "string" ? data.error : "Moderator service did not respond. Try again.");
      error.status = response.status;
      throw error;
    }
    return data;
  }
  function modCanManage() {
    return modDetails.open && !!modQueue && modFresh && socketConnected && state.online && !modBusy;
  }
  function renderModerator() {
    if (!modQueue || !modDetails.open) return;
    $("mod-panel").hidden = false;
    $("mod-login").hidden = true;
    $("mod-retry").hidden = modFresh;
    $("mod-who").textContent = "Signed in as " + modLogin;
    if (modFresh) $("mod-status").textContent = !socketConnected ? "Live updates disconnected. Management will return after reconnection."
      : !state.online ? "The engine is offline. Management will return when it reconnects."
      : "Moderator access authorized.";
    $("mod-add").disabled = !modCanManage() || !modAllowed("queue.request");
    $("mod-text").disabled = modBusy || !modAllowed("queue.request");
    $("mod-refresh").disabled = !!modRead || modBusy;
    $("mod-logout").disabled = modBusy;
    const any = ["queue.request", "queue.reorder", "queue.remove", "queue.approve", "queue.reject"].some(modAllowed);
    $("mod-permissions").hidden = any;
    $("mod-permissions").textContent = "Your account is authorized, but queue management actions are not enabled by the engine.";
    const key = JSON.stringify([modQueue.upcoming, modQueue.pending, modActions, modCanManage()]);
    if (key === modListKey || modDrag) return;
    modListKey = key;
    const renderedOrder = queueIds(modQueue);
    function button(label, action, args, title) {
      const b = modElement("button", "mod-button", label);
      b.type = "button"; b.disabled = !modCanManage() || !modAllowed(action);
      b.setAttribute("aria-label", label + " · " + title);
      b.onclick = () => modCommand(action, args, action === "queue.reorder" ? renderedOrder : null);
      return b;
    }
    function list(id, entries, pending) {
      const ol = $(id); ol.replaceChildren();
      entries.forEach((e, i) => {
        const li = modElement("li", "mod-row");
        const text = modElement("div");
        const title = songOf(e) || "Untitled request";
        text.append(modElement("div", "mod-title", (pending ? "#" + e.id : (i + 1) + ".") + " " + title));
        text.append(modElement("div", "mod-sub", [artistOf(e), e.user ? "requested by " + e.user : "", fmt(e.duration)].filter(Boolean).join(" · ")));
        const controls = modElement("div", "mod-actions");
        if (!pending) {
          const handle = modElement("span", "mod-drag", "Drag");
          handle.setAttribute("aria-hidden", "true");
          handle.draggable = modCanManage() && modAllowed("queue.reorder");
          handle.ondragstart = (ev) => {
            if (!modCanManage() || !modAllowed("queue.reorder")) { ev.preventDefault(); return; }
            modDrag = { id: e.id, order: queueIds(modQueue), index: i };
            li.classList.add("dragging");
            ev.dataTransfer.effectAllowed = "move";
            ev.dataTransfer.setData("text/plain", String(e.id));
          };
          handle.ondragend = () => {
            modDrag = null;
            modListKey = "";
            renderModerator();
          };
          li.ondragover = (ev) => {
            if (!modDrag || modDrag.id === e.id || !modCanManage()) return;
            ev.preventDefault(); ev.dataTransfer.dropEffect = "move";
            const rect = li.getBoundingClientRect();
            const after = ev.clientY > rect.top + rect.height / 2;
            document.querySelectorAll(".mod-row.drop-before,.mod-row.drop-after").forEach((r) => r.classList.remove("drop-before", "drop-after"));
            li.classList.add(after ? "drop-after" : "drop-before");
          };
          li.ondragleave = () => li.classList.remove("drop-before", "drop-after");
          li.ondrop = (ev) => {
            if (!modDrag || modDrag.id === e.id || !modCanManage()) return;
            ev.preventDefault();
            const drag = modDrag, rect = li.getBoundingClientRect();
            const after = ev.clientY > rect.top + rect.height / 2;
            // Destination is 1-based, after removing the dragged row.
            const to = i + (after ? 1 : 0) - (drag.index < i ? 1 : 0) + 1;
            modDrag = null; modListKey = "";
            if (to !== drag.index + 1) modCommand("queue.reorder", { id: drag.id, to }, drag.order);
            else renderModerator();
          };
          const up = button("Up", "queue.reorder", { id: e.id, to: i }, title);
          const down = button("Down", "queue.reorder", { id: e.id, to: i + 2 }, title);
          up.disabled = up.disabled || i === 0;
          down.disabled = down.disabled || i === entries.length - 1;
          controls.append(handle, up, down);
        } else controls.append(button("Approve", "queue.approve", { id: e.id }, title));
        controls.append(button("Reject", "queue.reject", { id: e.id }, title), button("Remove", "queue.remove", { id: e.id }, title));
        li.append(text, controls); ol.append(li);
      });
      if (!entries.length) ol.append(modElement("li", "empty", pending ? "No requests awaiting approval." : "No songs waiting."));
    }
    list("mod-waiting", modQueue.upcoming, false);
    list("mod-pending", modQueue.pending, true);
  }
  function refreshModerator() {
    if (!modDetails.open || modBlocked) return Promise.resolve(false);
    if (modRead) return modRead;
    const epoch = modEpoch, controller = new AbortController();
    modController = controller;
    const timeout = setTimeout(() => controller.abort(), 15000);
    modRead = (async () => {
      try {
        const data = await modPost({ kind: "query", name: "queue" }, controller.signal);
        if (epoch !== modEpoch || !modDetails.open) return false;
        if (!data.result || !Array.isArray(data.result.upcoming) || !Array.isArray(data.result.pending) || typeof data.login !== "string" || !Array.isArray(data.actions)) {
          throw new Error("Moderator service returned an incomplete queue. Try refreshing.");
        }
        modQueue = data.result;
        modActions = data.actions.filter((a) => typeof a === "string");
        modLogin = data.login; modFresh = true;
        return true;
      } catch (error) {
        if (epoch !== modEpoch || !modDetails.open) return false;
        modFresh = false;
        if (!modAccessError(error)) {
          $("mod-status").textContent = "Management unavailable: " + (error.name === "AbortError" ? "The connection timed out." : error.message);
          $("mod-retry").hidden = false;
        }
        return false;
      } finally {
        clearTimeout(timeout);
        if (epoch === modEpoch) {
          modRead = null; modController = null;
          renderModerator();
        }
      }
    })();
    renderModerator();
    return modRead;
  }
  async function modCommand(action, args, order) {
    if (!modCanManage() || !modAllowed(action)) return false;
    const epoch = modEpoch;
    modBusy = true; modMessage("Checking the latest queue…"); renderModerator();
    try {
      if (modRead) await modRead;
      if (!(await refreshModerator()) || epoch !== modEpoch || !modDetails.open) {
        if (epoch === modEpoch) modMessage("Command not sent. Refresh moderator access and try again.", true);
        return false;
      }
      if (!socketConnected || !state.online || !modAllowed(action)) throw new Error("Management is unavailable. Reconnect and try again.");
      if (order !== null && order !== undefined && queueIds(modQueue) !== order) throw new Error("The waiting queue changed. Review the refreshed order and try again.");
      if (args.id !== undefined) {
        const entries = action === "queue.approve" ? modQueue.pending
          : action === "queue.reorder" ? modQueue.upcoming : modQueue.upcoming.concat(modQueue.pending);
        if (!entries.some((e) => e.id === args.id)) throw new Error("That request is no longer waiting. The queue has been refreshed.");
      }
      const controller = new AbortController();
      const timeout = setTimeout(() => controller.abort(), 15000);
      try { await modPost({ kind: "cmd", action, args }, controller.signal); }
      finally { clearTimeout(timeout); }
      if (epoch !== modEpoch || !modDetails.open) return false;
      // Engine commands are asynchronous; an acknowledgement is not proof of addition.
      modMessage(action === "queue.request" ? "Request submitted. Lookup and queue policy still apply; watch for it in the refreshed queue."
        : "Command submitted. The queue will refresh as the engine processes it.");
      await refreshModerator();
      return true;
    } catch (error) {
      if (epoch === modEpoch && modDetails.open) {
        if (!modAccessError(error)) modMessage(error.name === "AbortError"
          ? "Command result unknown: the connection timed out. Refresh the queue before submitting again."
          : error.message || "Network error. Refresh before trying again.", true);
        if (!modBlocked) await refreshModerator();
      }
      return false;
    } finally {
      modBusy = false;
      renderModerator();
    }
  }
  function moderatorLiveChange() {
    renderModerator();
    if (!modDetails.open || modBlocked || modBusy || modLiveTimer) return;
    modLiveTimer = setTimeout(() => { modLiveTimer = null; refreshModerator(); }, 350);
  }
  function closeModerator() {
    modEpoch++;
    if (modController) modController.abort();
    clearInterval(modTimer); clearTimeout(modLiveTimer);
    modTimer = modLiveTimer = modRead = modController = null;
    modQueue = null; modActions = []; modLogin = ""; modFresh = false; modDrag = null; modListKey = "";
    $("mod-panel").hidden = true;
  }
  function openModerator() {
    modBlocked = false;
    $("mod-status").textContent = "Checking moderator access…";
    $("mod-login").hidden = $("mod-retry").hidden = true;
    refreshModerator();
    clearInterval(modTimer);
    modTimer = setInterval(() => { if (!modBusy) refreshModerator(); }, 5000);
  }
  modDetails.addEventListener("toggle", () => modDetails.open ? openModerator() : closeModerator());
  $("mod-retry").onclick = () => { modBlocked = false; refreshModerator(); };
  $("mod-refresh").onclick = () => refreshModerator();
  $("mod-form").onsubmit = async (ev) => {
    ev.preventDefault();
    const text = $("mod-text").value.trim();
    if (!text) return;
    if (await modCommand("queue.request", { text, user: modLogin }, null)) $("mod-text").value = "";
  };
  $("mod-logout").onclick = async () => {
    if (modBusy) return;
    modBusy = true; renderModerator();
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 15000);
    try {
      const response = await fetch("/mod/logout", { method: "POST", credentials: "same-origin", signal: controller.signal, headers: { "X-SE-Mod": "1" } });
      if (!response.ok) throw new Error("Sign out failed. Try again.");
      closeModerator();
      modAccessError({ status: 401 });
    } catch (error) { modMessage(error.name === "AbortError" ? "Sign out timed out. Try again." : error.message || "Network error while signing out.", true); }
    finally { clearTimeout(timeout); modBusy = false; renderModerator(); }
  };
  function moderatorHash() { if (location.hash === "#moderator") modDetails.open = true; }
  window.addEventListener("hashchange", moderatorHash);
  moderatorHash();
  let attempt = 0;
  function connect() {
    const ws = new WebSocket((location.protocol === "https:" ? "wss://" : "ws://") + location.host + "/queue/ws");
    let ping = null;
    ws.onopen = () => { socketConnected = true; attempt = 0; ping = setInterval(() => ws.readyState === 1 && ws.send("ping"), 30000); };
    ws.onmessage = (ev) => {
      if (ev.data === "pong") return;
      let m; try { m = JSON.parse(ev.data); } catch { return; }
      if (typeof m.now === "number") skew = m.now - Date.now();
      if (m.t === "queue") state = { online: !!m.online, snapshot: m.snapshot || null };
      else if (m.t === "status") state.online = !!m.online;
      render();
      moderatorLiveChange();
    };
    ws.onclose = () => {
      clearInterval(ping);
      socketConnected = false;
      renderModerator();
      $("status").className = "chip status";
      $("status-text").textContent = "Reconnecting";
      setTimeout(connect, Math.min(30000, 1000 * 2 ** attempt++));
    };
  }
  setInterval(tick, 500);
  render();
  connect();
})();
`;

// Cache-busting script URL: the page and script must change together behind CDN caches.
let scriptHash = 2166136261;
for (let i = 0; i < SCRIPT.length; i++) scriptHash = Math.imul(scriptHash ^ SCRIPT.charCodeAt(i), 16777619);
const SCRIPT_VERSION = (scriptHash >>> 0).toString(36);

export function queuePage(env: { QUEUE_TITLE?: string; QUEUE_CHANNEL?: string }): Response {
  const title = escapeHtml(env.QUEUE_TITLE || "Song queue");
  const channel = /^[A-Za-z0-9_]{3,25}$/.test(env.QUEUE_CHANNEL ?? "") ? (env.QUEUE_CHANNEL as string).toLowerCase() : "";
  const twitch = channel ? `https://www.twitch.tv/${channel}` : "";
  const html = `<!doctype html>
<html lang="en" data-theme="win31"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
<meta name="color-scheme" content="light"><meta name="theme-color" content="#008080">
<title>${title}</title><style>${STYLE}</style></head>
<body>
<div class="ambient" aria-hidden="true"><img id="ambient" alt=""></div>
<main>
<header class="top">
  <div>${channel ? `<a class="eyebrow" href="${twitch}" target="_blank" rel="noopener">twitch.tv/${channel}</a>` : `<span class="eyebrow">Live song requests</span>`}<h1>${title}</h1></div>
  <div class="chips"><span id="requests" class="chip" hidden><span class="dot"></span><span id="requests-text"></span></span><span id="status" class="chip status"><span class="dot"></span><span id="status-text">Connecting</span></span></div>
</header>
<section class="stage card" id="stage" aria-live="polite" aria-label="Now playing" hidden>
  <div class="window-title" aria-hidden="true">Media Player</div>
  <div class="cover" id="cover"><span class="note" aria-hidden="true">♪</span><img id="art" alt=""></div>
  <div class="info">
    <div class="kicker"><span class="eq" aria-hidden="true"><i></i><i></i><i></i><i></i></span><span id="kicker">Now playing</span></div>
    <h2 class="song" id="song"></h2>
    <p class="artist" id="artist"></p>
    <p class="album" id="album"></p>
    <div class="requester"><span class="avatar" id="avatar" aria-hidden="true"></span><span>requested by <b id="req-name"></b></span></div>
    <div class="progress" id="progress" role="progressbar" aria-label="Song progress" aria-valuemin="0" aria-valuemax="100"><div class="rail"><i id="bar"></i></div><div class="time"><span id="pos">0:00</span><span id="dur">0:00</span></div></div>
  </div>
</section>
<section class="idle card" id="idle" aria-live="polite">
  <div class="window-title" aria-hidden="true">Media Player</div>
  <div class="disc" aria-hidden="true"></div>
  <div><h2 id="idle-title">Connecting…</h2><p id="idle-text">Loading the queue.</p></div>
</section>
<section class="upnext card" aria-label="Up next">
  <div class="up-head"><h3>Up next</h3><span class="up-meta" id="up-meta"></span></div>
  <ol class="list" id="upcoming"></ol>
  <div class="cta" id="cta"><div class="cta-text"><span id="cta-label">Request a song in chat</span><code>!sr song or YouTube link</code></div>${twitch ? `<a class="btn" href="${twitch}" target="_blank" rel="noopener">Open chat ↗</a>` : ""}</div>
</section>
<details id="moderator" class="moderator card">
  <summary>Moderator login</summary>
  <div class="mod-body">
    <p>Optional · Broadcasters and channel moderators can manage requests here. Everyone can view the queue without an account.</p>
    <p id="mod-status" role="status">Open this section to check moderator access.</p>
    <a id="mod-login" class="mod-login" href="/mod/login" hidden>Sign in with Twitch</a>
    <button id="mod-retry" class="mod-button" type="button" hidden>Retry access</button>
    <div id="mod-panel" hidden>
      <div class="mod-toolbar"><span id="mod-who"></span><div class="mod-actions"><button id="mod-refresh" class="mod-button" type="button">Refresh</button><button id="mod-logout" class="mod-button" type="button">Sign out</button></div></div>
      <form id="mod-form" class="mod-form">
        <label class="mod-field" for="mod-text">Add a song or YouTube link<input id="mod-text" class="mod-input" type="text" placeholder="Song name or YouTube link" autocomplete="off" required></label>
        <button id="mod-add" class="mod-button" type="submit">Add song</button>
      </form>
      <p id="mod-permissions" hidden></p>
      <p class="mod-message" id="mod-message" role="status" aria-live="polite"></p>
      <section class="mod-section" aria-labelledby="mod-waiting-title"><h3 id="mod-waiting-title">Waiting queue</h3><p>Move songs with Up / Down, or drag the handle to reorder. The current song is not changed here.</p><ol id="mod-waiting" class="mod-list"></ol></section>
      <section class="mod-section" aria-labelledby="mod-pending-title"><h3 id="mod-pending-title">Pending approval</h3><p>Approve a request to put it in the waiting queue, where it can be reordered.</p><ol id="mod-pending" class="mod-list"></ol></section>
    </div>
  </div>
</details>
<footer>Updates live · Album art via Apple Music</footer>
</main><script src="/queue.js?v=${SCRIPT_VERSION}"></script></body></html>`;
  return new Response(html, {
    headers: {
      "content-type": "text/html; charset=utf-8",
      "content-security-policy": CSP,
      "x-content-type-options": "nosniff",
      "referrer-policy": "no-referrer",
      "cache-control": "public, max-age=60",
    },
  });
}

export function queueScript(): Response {
  return new Response(SCRIPT, {
    headers: { "content-type": "text/javascript; charset=utf-8", "x-content-type-options": "nosniff", "cache-control": "public, max-age=300" },
  });
}
