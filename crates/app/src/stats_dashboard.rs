//! Immutable embedded stats dashboard assets.

/// Versioned immutable dashboard asset.
pub struct Asset {
	/// HTTP content type.
	pub content_type: &'static str,
	/// Cache validator tied to the embedded payload.
	pub etag:         &'static str,
	/// Asset bytes.
	pub bytes:        &'static [u8],
}

const DASHBOARD: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="color-scheme" content="light dark"><title>OMP statistics</title>
<style>
:root{font:14px/1.45 system-ui,sans-serif;color-scheme:light dark;--bg:#f5f6f8;--panel:#fff;--ink:#17191d;--muted:#69707c;--line:#dfe2e8;--accent:#1769e0}html[data-theme=dark]{--bg:#111318;--panel:#191c22;--ink:#eef0f4;--muted:#a5abb6;--line:#303640;--accent:#75aaff}*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink)}header{position:sticky;top:0;z-index:2;display:flex;gap:12px;align-items:center;padding:14px 20px;border-bottom:1px solid var(--line);background:var(--panel)}header strong{font-size:18px}button,select{border:1px solid var(--line);border-radius:7px;background:var(--panel);color:var(--ink);padding:7px 10px}button{cursor:pointer}.spacer{flex:1}nav{display:flex;gap:4px;overflow:auto;padding:10px 20px;border-bottom:1px solid var(--line)}nav button.active{background:var(--accent);color:white;border-color:var(--accent)}main{max-width:1200px;margin:auto;padding:20px}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(190px,1fr));gap:12px}.card{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:16px}.metric{font-size:26px;font-weight:650}.muted{color:var(--muted)}table{width:100%;border-collapse:collapse;background:var(--panel)}th,td{text-align:left;padding:9px;border-bottom:1px solid var(--line)}pre{white-space:pre-wrap;overflow-wrap:anywhere}.state{padding:36px;text-align:center;background:var(--panel);border:1px solid var(--line);border-radius:10px}.error{color:#d33}@media(max-width:600px){header{padding:10px}nav,main{padding:10px}}
</style></head><body><header><strong>OMP statistics</strong><span id="status" class="muted"></span><span class="spacer"></span><select id="range" aria-label="Time range"><option value="24h">24 hours</option><option value="7d">7 days</option><option value="30d" selected>30 days</option><option value="90d">90 days</option><option value="all">All time</option></select><button id="sync">Sync</button><button id="theme" aria-label="Toggle theme">Theme</button></header><nav id="nav"></nav><main id="main"><div class="state">Loading statistics...</div></main>
<script>
const routes={overview:'overview',requests:'recent',errors:'errors',models:'models',providers:'providers',tools:'tools',costs:'costs',behavior:'behavior',projects:'folders',gain:'gain'};let controller;
const $=id=>document.getElementById(id), esc=s=>String(s??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
function route(){const key=location.hash.slice(1);return routes[key]?key:'overview'}
function headers(){const token=sessionStorage.getItem('omp-stats-token');return token?{accept:'application/json',authorization:`Bearer ${token}`}:{accept:'application/json'}}
function nav(){const active=route();$('nav').innerHTML=Object.keys(routes).map(k=>`<button data-route="${k}" class="${k===active?'active':''}">${k[0].toUpperCase()+k.slice(1)}</button>`).join('');document.querySelectorAll('[data-route]').forEach(b=>b.onclick=()=>location.hash=b.dataset.route)}
function cards(data){const o=data.overall||data;const entries=Object.entries(o).filter(([,v])=>['number','string'].includes(typeof v));if(!entries.length)return '<div class="state">No statistics in this range.</div>';return `<div class="grid">${entries.map(([k,v])=>`<section class="card"><div class="muted">${esc(k.replaceAll('_',' '))}</div><div class="metric">${esc(v)}</div></section>`).join('')}</div>`}
function table(data){const rows=Array.isArray(data)?data:data.rows||data.items||[];if(!rows.length)return '<div class="state">No records in this range.</div>';const cols=[...new Set(rows.flatMap(Object.keys))];return `<table><thead><tr>${cols.map(c=>`<th>${esc(c)}</th>`).join('')}</tr></thead><tbody>${rows.map(r=>`<tr>${cols.map(c=>`<td>${typeof r[c]==='object'?`<pre>${esc(JSON.stringify(r[c],null,2))}</pre>`:esc(r[c])}</td>`).join('')}</tr>`).join('')}</tbody></table>`}
async function load(){controller?.abort();controller=new AbortController();nav();$('main').innerHTML='<div class="state">Loading statistics...</div>';const key=route(),range=$('range').value;try{const r=await fetch(`/api/v1/stats/${routes[key]}?range=${range}`,{signal:controller.signal,headers:headers()});if(r.status===401){const token=prompt('Statistics access token');if(token){sessionStorage.setItem('omp-stats-token',token);return load()}}if(!r.ok)throw new Error(`HTTP ${r.status}`);const envelope=await r.json();$('status').textContent=envelope.meta?.range||range;$('main').innerHTML=key==='overview'?cards(envelope.data):table(envelope.data)}catch(e){if(e.name!=='AbortError')$('main').innerHTML=`<div class="state error">Could not load statistics: ${esc(e.message)}<br><button onclick="load()">Retry</button></div>`}}
$('sync').onclick=async()=>{const b=$('sync');b.disabled=true;b.textContent='Syncing...';try{const r=await fetch('/api/v1/stats/sync',{method:'POST',headers:headers()});if(!r.ok)throw new Error(`HTTP ${r.status}`);await load()}catch(e){$('main').innerHTML=`<div class="state error">Sync failed: ${esc(e.message)}</div>`}finally{b.disabled=false;b.textContent='Sync'}};
$('range').onchange=load;$('theme').onclick=()=>{const root=document.documentElement;const next=root.dataset.theme==='dark'?'light':'dark';root.dataset.theme=next;localStorage.setItem('omp-stats-theme',next)};const saved=localStorage.getItem('omp-stats-theme');if(saved)document.documentElement.dataset.theme=saved;addEventListener('hashchange',load);load();
</script></body></html>"##;

/// Looks up an embedded production dashboard asset.
#[must_use]
pub fn asset(path: &str) -> Option<Asset> {
	match path {
		"/" | "/index.html" => Some(Asset {
			content_type: "text/html; charset=utf-8",
			etag:         "\"omp-stats-dashboard-v1\"",
			bytes:        DASHBOARD.as_bytes(),
		}),
		_ => None,
	}
}
