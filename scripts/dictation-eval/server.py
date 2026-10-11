"""Serves the eval page; logs a settings snapshot, then per phrase: as-inserted text, fixed text and timed input events."""
import calendar, json, os, pathlib, re, shutil, sqlite3, subprocess, sys, time
import grade
from http.server import BaseHTTPRequestHandler, HTTPServer

D = pathlib.Path(__file__).parent
STORE = pathlib.Path.home() / "Library/Application Support/com.pais.handy/settings_store.json"
HANDY = pathlib.Path(os.environ.get("HANDY_BIN", D.parents[1] / "src-tauri/target/debug/handy"))
SRC = pathlib.Path(sys.argv[2]) if len(sys.argv) > 2 else None  # replay: re-run the audio saved in this run
HIST = STORE.parent / "history.db"
FLAGS = ("auto_learn_enabled", "live_typing_enabled", "post_process_enabled", "scratch_that_enabled", "selected_model",
         "post_process_selected_prompt_id", "word_correction_threshold", "append_trailing_space", "history_limit")
LOG = pathlib.Path.home() / "Library/Logs/com.pais.handy/handy.log"
LOG_TS = re.compile(r"^\[(\d{4}-\d{2}-\d{2})\]\[(\d{2}:\d{2}:\d{2})\]")

def snap():
    """Only flags and word lists: the store also holds API keys."""
    s = json.loads(STORE.read_text()); s = s.get("settings", s)
    return {"flags": {k: s.get(k) for k in FLAGS}, "custom_words": s.get("custom_words", []),
            "pairs": [[r["from"], r["to"], r.get("learned", False)] for r in s.get("word_replacements", [])]}

USED = set()

def history_entry(shown_at, phrase=None):
    """Unclaimed Handy history entry since this phrase was shown: the one most like `phrase`, else the newest
    (an empty one is legit: scratch-that can delete everything)."""
    db = sqlite3.connect(f"file:{HIST}?mode=ro", uri=True)
    marks = ",".join("?" * len(USED))
    rows = db.execute("select id, file_name, transcription_text, post_processed_text from transcription_history "
                      f"where timestamp >= ? and id not in ({marks or '-1'}) order by timestamp desc", (shown_at // 1000 - 2, *USED)).fetchall()
    if phrase and rows:
        return max(rows, key=lambda row: grade.difflib.SequenceMatcher(None, grade.loose(row[2] or ""), grade.loose(phrase)).ratio())
    return rows[0] if rows else None

def log_lines(shown_at):
    """Handy log lines since the phrase was shown (log times are UTC), minus enigo's per-keystroke noise: the log rotates fast."""
    try:
        out = []
        for line in LOG.read_text(errors="replace").splitlines():
            m = LOG_TS.match(line)
            if m and calendar.timegm(time.strptime(" ".join(m.groups()), "%Y-%m-%d %H:%M:%S")) >= shown_at // 1000 and "][enigo" not in line:
                out.append(line)
        return out
    except OSError:
        return []

def capture(r):
    """Handy keeps each dictation's WAV + raw ASR + post-processed text in its history; copy them now, retention may delete them later.
    A row that does not resemble the phrase belongs to another dictation (a restart, a stray recording): rejected, not copied."""
    logs = OUT.with_suffix("") / "logs"
    logs.mkdir(parents=True, exist_ok=True)
    (logs / f"{r['id']}.log").write_text("\n".join(log_lines(r["shown_at"])) + "\n")
    try:
        row = history_entry(r["shown_at"], r["phrase"])
        if not row: return {"capture_error": "no history entry"}
        if not grade.resembles(row[2], r["phrase"]) and (row[2] or r["expected"]):
            return {"capture_error": f"history entry {row[0]} is another dictation"}
        USED.add(row[0])
        wav = OUT.with_suffix("") / "audio" / f"{r['id']}.wav"
        wav.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy(STORE.parent / "recordings" / row[1], wav)
        return {"history_id": row[0], "audio": str(wav.relative_to(D)), "raw_asr": row[2], "handy_pp": row[3]}
    except Exception as e:
        return {"capture_error": str(e)}

(D / "runs").mkdir(exist_ok=True)
label = sys.argv[1] if len(sys.argv) > 1 else "run"
OUT = D / "runs" / f"{time.strftime('%Y%m%d-%H%M%S')}_{label}.jsonl"
OUT.write_text(json.dumps({"meta": {"label": label, "start": snap(), "replay_of": str(SRC) if SRC else None}}) + "\n")
if SRC:  # one row per phrase whose audio was kept, same fields the page needs
    PHRASES = [r for r in map(json.loads, SRC.read_text().splitlines()[1:]) if r.get("audio") and not r["skipped"]]
    PHRASES = [{k: r[k] for k in ("id", "category", "phrase", "expected", "term", "audio")} for r in PHRASES]
else:
    PHRASES = [p for p in json.loads((D / "phrases.json").read_text()) if p.get("suite") == label]  # label = plain | pp
PAGE = """<!doctype html><meta charset=utf-8><title>Dictation eval</title>
<style>body{font:20px system-ui;max-width:760px;margin:48px auto;padding:0 16px}
#ph{font-size:30px;line-height:1.35;margin:12px 0 16px}#res{min-height:5em;font-size:16px;margin-top:14px;line-height:1.5}
input{width:100%;font-size:22px;padding:12px;box-sizing:border-box}small{color:#666}button{font-size:16px;margin-top:12px}
b.ok{color:#15803d}b.loose{color:#b45309}b.bad{color:#b91c1c}code{background:#f3f4f6;padding:1px 4px}</style>
<small id=meta></small><div id=ph></div>
<input id=box autofocus autocomplete=off autocorrect=off spellcheck=false>
<div id=res></div><button id=skip>Skip</button> <small>Suite: <b id=suite></b>. Hands-free: it checks once Handy has saved the dictation and 1 s passed without new text (?wait=ms); Enter = check now, then Enter = next phrase. Replay mode (?replay=1) runs by itself.</small>
<script>
let shownAt=0,P=[],i=0,ev=[],t0=0,frozen=false,timer=null,doneAt=0,ready=false,cur={},playP=null;const $=id=>document.getElementById(id);
const SUITE='__SUITE__',Q=new URLSearchParams(location.search),REPLAY=Q.has('replay'),WAIT=+Q.get('wait')||(REPLAY?2500:1000);
const sleep=ms=>new Promise(r=>setTimeout(r,ms));const esc=s=>s.replace(/[&<>]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;'}[c]));
document.getElementById('suite').textContent=SUITE==='pp'?'PP: use option+shift+space':SUITE==='plain'?'PLAIN: use option+space':SUITE;
function show(){ev=[];t0=0;frozen=false;ready=false;doneAt=0;clearTimeout(timer);$('box').value='';$('res').innerHTML='';
 if(i>=P.length){$('ph').textContent='Done.';$('meta').textContent='';return}
 shownAt=Date.now();const p=P[i];$('meta').textContent=`${i+1}/${P.length}${REPLAY?' · replay':''}`;$('ph').textContent=p.phrase;$('box').focus();
 arm();if(REPLAY){playP=fetch('/api/play',{method:'POST',body:JSON.stringify(p)}).then(r=>r.json()).then(r=>{if(r.rc)$('res').textContent='handy exited '+r.rc+': '+r.stderr;if(!frozen)setTimeout(()=>freeze(true),WAIT)})}}  // replay saves no history: check once Handy exits
$('box').addEventListener('input',e=>{if(frozen)return;const n=performance.now();if(!t0)t0=n;ev.push({t:Math.round(n-t0),v:e.target.value});arm()});
function arm(){clearTimeout(timer);if(!frozen)timer=setTimeout(()=>freeze(false),WAIT)}
// Handy grabs option+space before the page sees it, so key presses can't tell when a dictation ends. Instead the
// page waits for Handy to save the dictation to its history (that happens right before the final text is typed),
// then for WAIT ms without new text. Enter = check now.
document.addEventListener('keydown',e=>{if(e.code==='Space'&&e.altKey)e.preventDefault()}); // would type a non-breaking space
async function freeze(force){if(frozen)return;
 if(!force){const d=await(await fetch('/api/ready',{method:'POST',body:JSON.stringify({...P[i],shown_at:shownAt})})).json();
  if(frozen)return;
  if(!d.ready){doneAt=0;$('res').textContent=$('box').value?'':'waiting for the dictation… (Skip if nothing comes)';arm();return}
  if(!doneAt){doneAt=Date.now();arm();return}} // saved: give the final text one more quiet WAIT to land
 frozen=true;clearTimeout(timer);const p=P[i],ins=$('box').value,insT=t0?Math.round(performance.now()-t0):0;
 const r=await(await fetch('/api/freeze',{method:'POST',body:JSON.stringify({...p,as_inserted:ins,shown_at:shownAt})})).json();
 const cls={PASS:'ok',LOOSE:'loose',FAIL:'bad'}[r.verdict];
 $('res').innerHTML=`<b class=${cls}>${r.verdict}</b> <small>Enter = next</small><br>expected <code>${esc(p.expected)||'(empty)'}</code><br>got <code>${esc(ins)||'(empty)'}</code>`+(SUITE==='pp'&&r.pp_ran===false?'<br><b class=bad>post-processing did NOT run: use option+shift+space</b>':'')+(SUITE==='plain'&&r.pp_ran===true?'<br><b class=bad>post-processing ran: this suite is plain, use option+space</b>':'')+(r.fixed!==ins?`<br>auto-fix for auto-learn: <code>${esc(r.fixed)}</code>`:'');
 if(r.fixed!==ins)$('box').value=r.fixed;
 cur={ins,insT,fixT:performance.now()};ready=true;
 if(REPLAY){await playP;await sleep(1500);next()}}
async function next(){ready=false;await sleep(Math.max(0,1500-(performance.now()-cur.fixT))); // auto-learn polls the field every 500 ms
 await save(false,cur.ins,cur.insT)}
async function save(skipped,ins,insT){const p=P[i];
 await fetch('/api/result',{method:'POST',body:JSON.stringify({...p,as_inserted:ins??$('box').value,typed:$('box').value,ins_t:insT||0,shown_at:shownAt,skipped,events:ev})});i++;show()}
$('box').addEventListener('keydown',e=>{if(e.key==='Enter'){e.preventDefault();if(!frozen)freeze(true);else if(ready)next()}});$('skip').onclick=()=>{frozen=true;save(true)};
fetch('/api/phrases').then(r=>r.json()).then(p=>{P=p;show()});
</script>"""

class H(BaseHTTPRequestHandler):
    def _send(self, body, ctype):
        b = body.encode(); self.send_response(200)
        self.send_header("Content-Type", ctype); self.send_header("Content-Length", str(len(b)))
        self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        if self.path == "/api/phrases": self._send(json.dumps(PHRASES), "application/json")
        else: self._send(PAGE.replace("__SUITE__", label), "text/html; charset=utf-8")
    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"])).decode()
        if self.path == "/api/play":  # types the saved WAV into the focused box like a dictation; blocks until Handy exits
            subprocess.run(["open", "-a", "Google Chrome"]); time.sleep(0.5)
            p = subprocess.run([str(HANDY), "--transcribe-file", str(D / json.loads(body)["audio"]), "--stream", "--live-type", "6"],
                               capture_output=True, text=True, timeout=180, cwd=D.parents[1] / "src-tauri")  # debug builds write ../src/bindings.ts
            return self._send(json.dumps({"rc": p.returncode, "stderr": p.stderr[-300:]}), "application/json")
        if self.path == "/api/ready":  # the dictation for this phrase is in Handy's history
            r = json.loads(body)
            try: row = history_entry(r["shown_at"], r["phrase"])
            except Exception: row = None
            ok = bool(row) and (grade.resembles(row[2], r["phrase"]) or not (row[2] or r["expected"]))
            return self._send(json.dumps({"ready": ok}), "application/json")
        if self.path == "/api/freeze":
            r = json.loads(body)
            try: row = history_entry(r["shown_at"], r["phrase"])
            except Exception: row = None
            fix_term = r.get("term") if r.get("category") != "guard" else None  # guard: a learned pair misfired; fixing it back would teach the reverse pair
            return self._send(json.dumps({"verdict": grade.verdict(r), "fixed": grade.auto_fix(r["as_inserted"], r["expected"], fix_term),
                                          "pp_ran": bool(row and row[3]) if row else None}), "application/json")
        r = json.loads(body)
        if not r["skipped"] and not r.get("audio"): r.update(capture(r))
        with open(OUT, "a") as f: f.write(json.dumps(r, ensure_ascii=False) + "\n")
        OUT.with_suffix(".end.json").write_text(json.dumps(snap()))  # pairs learned so far: trend must not drift with later runs
        self._send("{}", "application/json")
    def log_message(self, *a): pass

print(f"results -> {OUT}\n" + (f"REPLAY of {SRC.name}: {len(PHRASES)} phrases. Quit the Handy app first: it shares the settings file." if SRC else ""))
subprocess.Popen(["open", "-a", "Google Chrome", "http://127.0.0.1:8765" + ("?replay=1" if SRC else "")])
HTTPServer(("127.0.0.1", 8765), H).serve_forever()
