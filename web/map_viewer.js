(() => {
  const W=1885.618,H=1060.66;
  const $=id=>document.getElementById(id);
  const canvas=$('map'),ctx=canvas.getContext('2d');
  let captureId=null, decisions=[], snapshot=null, simulations=[],timeline=[],sel=null,selScope=null,snapshotRequest=0;
  let position='none',layer='gravity',botNames=new Map();
  const LAYER_NAMES={gravity:'Gravity belief',uncertainty:'Fit uncertainty',error:'Belief error'};
  // Perceptual sequential colour maps (matplotlib viridis and magma, 9 stops each) as 256-entry RGB tables.
  const RAMPS={viridis:['#440154','#482878','#3e4989','#31688e','#26828e','#1f9e89','#35b779','#6ece58','#fde725'],magma:['#000004','#1c1044','#4f127b','#812581','#b5367a','#e55964','#fb8761','#fec287','#fcfdbf']};
  const LUTS=Object.fromEntries(Object.entries(RAMPS).map(([name,stops])=>{const rgb=stops.map(h=>[1,3,5].map(i=>parseInt(h.slice(i,i+2),16))),lut=new Uint8Array(256*3);for(let i=0;i<256;i++){const f=i/255*(rgb.length-1),k=Math.min(rgb.length-2,Math.floor(f)),u=f-k;for(let c=0;c<3;c++)lut[i*3+c]=Math.round(rgb[k][c]*(1-u)+rgb[k+1][c]*u);}return [name,lut];}));
  const LAYER_RAMP={gravity:'viridis',uncertainty:'viridis',error:'magma'};
  // Path colours by the outcome inferred from each observed endpoint.
  const OUTCOME_COLORS={player:'#ff4d6a',planet:'#ffb347',left_field:'#dfe6f0',unknown:'#b98cff'},OUTCOME_LABELS={player:'hit a player',planet:'planet surface',left_field:'left the field',unknown:'unknown'};
  const STATUS_LABEL={unknown:'no comparable belief',ok:'close match',location:'center off',size:'size off',both:'center and size off',missing:'missing'},LOCATION_TOL=10,SIZE_TOL=5,STATUS_COLORS={unknown:'#60dcff',ok:'#60dcff',location:'#ff9d4d',size:'#e8d96d',both:'#ff7a5c',missing:'#ff4d6a'};
  let view={x:0,y:0,width:W,height:H}, viewDrag=null, panned=false, heatCache=null, pathCache=null;
  const colors=['#54c8ff','#ffb45c','#b791ff','#7de3a1','#ff7899','#e8d96d','#88a7ff','#ffa1dc'];
  function setStatus(s){ $('status').textContent=s||''; }
  function fmt(v,n=2){ return v===null||v===undefined||v===''?'—':Number.isFinite(Number(v))?Number(v).toFixed(n):'—'; }
  // The protocol identifies players by id only; our bots' names come from the decisions they recorded.
  const pname=id=>botNames.has(String(id))?`${id} ${botNames.get(String(id))}`:String(id);
  function selected(){ return Number(position); }
  function q(v){return encodeURIComponent(v);}
  async function api(path){const r=await fetch(path,{cache:'no-store'});const data=await r.json();if(!r.ok||data.error)throw new Error(data.error||r.statusText);return data;}
  // Fills the capture select; returns false when the database has no captures.
  async function fillCaptureSelect(){
    setStatus('Loading captures…');
    const data=await api('/api/captures'); const select=$('capture'); select.replaceChildren();
    for(const c of data.captures){const option=document.createElement('option');option.value=c.id;const date=new Date(c.started_at_unix_ms).toLocaleString();const kind=c.metadata?.host==='synthetic-scenario'?'scenario':'live',revision=c.metadata?.build?.git_revision,older=kind==='scenario'&&data.captures.some(o=>o.metadata?.host==='synthetic-scenario'&&o.metadata?.client_name===c.metadata?.client_name&&o.id>c.id),session=c.metadata?.session;option.textContent=`#${c.id} · ${kind}${older?' (older revision)':''} · ${c.metadata?.client_name||'capture'}${session?` · map ${session.map_index} of session ${session.id}`:''}${revision?` · ${revision.slice(0,8)}`:''} · ${date}`;select.append(option);}
    if(!data.captures.length){setStatus('No captures in this database');return false;}
    setStatus('');return true;
  }
  async function loadDecisions(preferredBot=null){
    captureId=Number($('capture').value);timeline=[];drawScrub(); const id=captureId,data=await api(`/api/decisions?id=${id}`); if(id!==captureId)return; decisions=data.decisions;botNames=new Map(decisions.filter(d=>d.own_id!=null&&d.bot_name!=='scenario').map(d=>[String(d.own_id),d.bot_name]));
    const bots=[...new Set(decisions.map(d=>d.bot_name))]; const bot=$('bot');const old=preferredBot||bot.value;bot.replaceChildren();
    for(const name of ['all',...bots]){const op=document.createElement('option');op.value=name;op.textContent=name==='all'?'All bots':name;bot.append(op);}
    bot.value=bots.includes(old)?old:'all'; fillDecisionList();
  }
  function fillDecisionList(){position='none';selScope=null;review=null;degrade=null;loadSnapshot();loadTimeline();loadPredictive();}
  // Field channels arrive as base64 little-endian f32 (NaN = unavailable; null = no value anywhere).
  function f32(b64){if(!b64)return null;const bin=atob(b64),bytes=new Uint8Array(bin.length);for(let i=0;i<bin.length;i++)bytes[i]=bin.charCodeAt(i);return new Float32Array(bytes.buffer);}
  async function loadSnapshot(){
    const request=++snapshotRequest,requestCapture=captureId,requestDecision=position,requestView={...view};
    setStatus('Loading captured state…');simulations=[];
    try{const step=gridStepForView(),params=`id=${requestCapture}&seq=${requestDecision}&step=${step}&x=${requestView.x}&y=${requestView.y}&width=${requestView.width}&height=${requestView.height}${$('pathResiduals').checked?'&residuals=1':''}`;const result=await api(`/api/snapshot?${params}`);if(request!==snapshotRequest||requestCapture!==captureId||requestDecision!==position||requestView.x!==view.x||requestView.y!==view.y||requestView.width!==view.width||requestView.height!==view.height)return;snapshot=result;heatCache=null;pathCache=null;const scope=`${captureId}|${requestDecision}`;if(scope!==selScope){selScope=scope;sel=snapshot.selected_shot?{kind:'shot',sequence:snapshot.selected_shot.sequence}:null;degrade=null;loadReview();}const g=snapshot.field_grid;for(const k of ['gravity_x','gravity_y','truth_x','truth_y','sigma'])g[k]=f32(g[k]);g.carried={};const c=currentDecision().choice||{};$('velocity').value=c.velocity||8;$('angle').value=c.angle_degrees||0;setStatus('');render();carryForward(request,requestCapture,requestDecision,params);}
    catch(e){setStatus(e.message);}
  }
  async function loadTimeline(){try{const id=captureId,data=await api(`/api/timeline?id=${id}`);if(id!==captureId)return;timeline=data.timeline;drawScrub();}catch(e){setStatus(e.message);}}
  function currentDecision(){return decisions.find(d=>d.sequence===selected())||{};}
  // About one field sample per three screen pixels: coarse at full view, finer when zoomed in.
  function gridStepForView(){const px=canvas.getBoundingClientRect().width||1200;return Math.max(1,Math.min(25,Math.ceil(view.width/(px/3))));}
  // The field at the grid sample nearest (x, y): belief pull, truth pull, their difference and σ.
  function gridAt(x,y){
    const g=snapshot?.field_grid;if(!g?.columns)return null;
    const col=Math.max(0,Math.min(g.columns-1,Math.round((x-g.origin_x)/g.step))),row=Math.max(0,Math.min(g.rows-1,Math.round((y-g.origin_y)/g.step))),i=row*g.columns+col;
    const pair=(a,b)=>a&&Number.isFinite(a[i])&&Number.isFinite(b[i])?{x:a[i],y:b[i]}:null,total=pair(g.gravity_x,g.gravity_y),oracle=pair(g.truth_x,g.truth_y);
    return {total,oracle,delta:total&&oracle?{x:total.x-oracle.x,y:total.y-oracle.y}:null,sigma:g.sigma&&Number.isFinite(g.sigma[i])?g.sigma[i]:null};
  }
  // The Error layer needs a truth map; without one the Gravity layer is shown instead.
  function shownLayer(){return layer==='error'&&!snapshot?.actual_planets?'gravity':layer;}
  // A decision whose belief has no covariance borrows the latest earlier one from the same map epoch (same planets message), clearly marked.
  // This is common: the belief published right after a path has none yet (not_computed); the refit with covariance follows.
  async function carryForward(request,requestCapture,requestDecision,params){
    const g=snapshot?.field_grid,now=decisions.find(d=>d.sequence===snapshot.diagnostic_sequence);if(!g||!now)return;
    if(g.sigma)return;
    const source=decisions.filter(d=>d.map_epoch===now.map_epoch&&d.sequence<now.sequence&&d.covariance).at(-1);if(!source)return;
    try{
      const old=await api(`/api/snapshot?${params.replace(/seq=[^&]*/,`seq=${source.sequence}`)}`);
      if(request!==snapshotRequest||requestCapture!==captureId||requestDecision!==position)return;
      const og=old.field_grid;if(!og||og.columns!==g.columns||og.rows!==g.rows)return;
      const sigma=f32(og.sigma);if(!sigma)return;
      g.sigma=sigma;g.carried.uncertainty=source.sequence;heatCache=null;render();
    }catch(e){setStatus(e.message);}
  }
  function carriedBanner(l){
    const sequence=snapshot?.field_grid?.carried?.[l],now=decisions.find(d=>d.sequence===snapshot.diagnostic_sequence);if(!sequence||!now)return null;
    const age=decisions.filter(d=>d.map_epoch===now.map_epoch&&d.sequence>sequence&&d.sequence<=now.sequence).length;
    return `Carried forward from decision #${sequence}, ${age} decision${age===1?'':'s'} earlier on this map. The current decision has none: ${covarianceNote()}.`;
  }
  const COVARIANCE_REASONS={not_computed:'not computed yet: this belief was published right after a new path, and the refit that supplies the covariance follows',no_planets:'the map has no planets, so there is no planet covariance (planets not yet found are not modelled)',under_identified:'the paths leave some planet parameters (nearly) undetermined, or a planet sits at a bound',no_planet_parameters:'the map has no fitted planet parameters',fit_failed:'the planet fit failed',topology_changed:'the planet set changed after the fit, so its covariance no longer applies',fit_did_not_converge:'the fit has not converged',parameter_transform_unavailable:'the fit parameter transform is unavailable',rank_deficient:'the fit is rank deficient: some planet parameters are not identified by the recorded paths',insufficient_residual_degrees_of_freedom:'too few residuals for the number of fitted parameters',insufficient_independent_paths:'too few independent paths'};
  // Why the fit covariance is missing; null when a covariance exists.
  function covarianceNote(){
    const b=snapshot?.diagnostic?.map_belief;if(!b||b.covariance)return null;
    return b.covariance_unavailable_reason?(COVARIANCE_REASONS[b.covariance_unavailable_reason]||b.covariance_unavailable_reason):'not recorded in this capture (it predates the covariance diagnostics)';
  }
  const SEARCH_STATES={settled:'settled: no birth or death proposal left',planet_cap:'stopped at the planet cap (a further planet may exist)',work_budget:'work left: a search slice reached its evaluation limit, or slices owed after the latest path have not run yet',time_budget:'cut: a search slice hit its wall-clock ceiling'};
  const searchText=state=>state==null?'not recorded (capture predates it)':SEARCH_STATES[state]||String(state);
  // How far the decision's belief trailed the observations: paths queued for (or in) the estimator when the decision read it.
  // Captures before map_pending_paths infer it from the recorded estimator inputs (marked ≈, may be one low with several bots).
  function lagText(d){const l=d?.lag;if(!l)return null;const approx=l.inferred?'≈':'';return l.paths?`lagged ${approx}${l.paths} path${l.paths===1?'':'s'} (${l.events} input${l.events===1?'':'s'} queued)`:l.events?`no path pending (${l.events} other input${l.events===1?'':'s'} queued)`:'up to date';}
  const PART_LABELS={noise:'noise model',warm_refit:'local refit',seeding:'birth seeds',birth_alone:'birth alone fits',birth_joint:'birth joint refits',deaths:'death trials',polish:'full-row refit',diagnostics:'diagnostics',window_build:'window build',contacts:'contacts',residual_eval:'residual eval',jacobian_eval:'Jacobian eval',lm_solve:'LM solve',evidence:'evidence',covariance:'covariance'};
  const PART_COLORS={noise:'#8e7cc3',warm_refit:'#4fa3e0',seeding:'#7de3a1',birth_alone:'#2aaa8c',birth_joint:'#1f7a68',deaths:'#e8d96d',polish:'#ffb347',diagnostics:'#8799af',window_build:'#4fa3e0',contacts:'#6ef0d0',residual_eval:'#b98cff',jacobian_eval:'#ff8fb1',lm_solve:'#ff9d4d',evidence:'#e8d96d',covariance:'#e8b84a'};
  // The estimator update that published the belief as two bars on one millisecond scale: phases (wall time, sequential) and leaf parts (CPU summed over threads, nested inside the phases).
  // Red outline: the part exceeded its ceiling; hatched end: a search slice hit its wall ceiling. Numbers are on hover.
  function renderUpdate(source){
    const el=$('updateReport');el.replaceChildren();const u=source?.update;
    if(!u){if(snapshot?.diagnostic){const n=document.createElement('div');n.className='note';n.textContent='Not recorded for this belief (captures before 2026-11-27, or an oracle-map decision).';el.append(n);}return;}
    const hit=new Map((u.violations||[]).map(v=>[v.part,v])),cut=source.search_state==='time_budget';
    const phases=u.parts.filter(p=>p.phase),leaves=u.parts.filter(p=>!p.phase),leafMs=leaves.reduce((t,p)=>t+(p.total_ms||0),0),scale=Math.max(u.phase_ms||0,leafMs,1e-9);
    const bar=(label,parts,total,hatched)=>{
      const row=document.createElement('div');row.className='ubar-row';const name=document.createElement('span');name.textContent=label;const track=document.createElement('div');track.className='ubar';
      for(const p of parts){if(!(p.total_ms>0))continue;const seg=document.createElement('div'),v=hit.get(p.part);seg.style.width=`${100*p.total_ms/scale}%`;seg.style.background=PART_COLORS[p.part]||'#8799af';if(v)seg.className='hit';seg.title=`${PART_LABELS[p.part]||p.part}: ${fmt(p.total_ms,1)} ms in ${p.calls} call${p.calls===1?'':'s'} (longest ${fmt(p.longest_ms,1)} ms)${v?` · exceeded its ceiling: ${fmt(v.observed_ms,0)} of ${fmt(v.ceiling_ms,0)} ms`:''}`;track.append(seg);}
      if(hatched){const seg=document.createElement('div');seg.className='cut';seg.title='A search slice hit its wall-clock ceiling (time cut): the search stopped early.';track.append(seg);}
      const ms=document.createElement('span');ms.textContent=`${fmt(total,0)} ms`;row.append(name,track,ms);return row;
    };
    const legend=document.createElement('div');legend.className='ulegend';
    for(const p of [...phases,...leaves]){if(!(p.total_ms>0))continue;const item=document.createElement('span'),sw=document.createElement('i');sw.style.background=PART_COLORS[p.part]||'#8799af';if(hit.has(p.part))sw.className='hit';item.append(sw,PART_LABELS[p.part]||p.part);item.title=`${fmt(p.total_ms,1)} ms · ${p.calls} calls`;legend.append(item);}
    el.title=`${u.evaluations} evaluations. Covers the latest path's local refit and the slices and refit after it, up to this belief.`;
    el.append(bar('wall',phases,u.phase_ms,cut),bar('CPU',leaves,leafMs,false),legend);
  }
  // The shown layer's scalar at grid index i: |gravity|, field σ, or |belief − truth|.
  function heatValue(g,l,i){
    if(l==='uncertainty')return g.sigma?g.sigma[i]:NaN;
    if(!g.gravity_x)return NaN;const x=g.gravity_x[i],y=g.gravity_y[i];
    if(l==='error')return g.truth_x?Math.hypot(x-g.truth_x[i],y-g.truth_y[i]):NaN;
    return Math.hypot(x,y);
  }
  // The heat layer as an image with one pixel per grid sample, rebuilt only when the snapshot, layer or carried σ changes.
  // Every layer spans orders of magnitude (1/r² near planets), so colours are log-scaled between the 2nd and 99.5th percentile.
  function heatLayer(){
    const g=snapshot?.field_grid,l=shownLayer(),key=`${l}|${g?.carried?.uncertainty||''}`;
    if(heatCache?.key===key)return heatCache;
    const n=(g?.columns||0)*(g?.rows||0),values=new Float32Array(n);let finite=[];
    for(let i=0;i<n;i++){const v=heatValue(g,l,i);values[i]=v;if(v>0&&Number.isFinite(v))finite.push(v);}
    finite.sort((a,b)=>a-b);const q=p=>finite[Math.min(finite.length-1,Math.floor((finite.length-1)*p))];
    heatCache={key,count:finite.length,lo:finite.length?q(.02):null,hi:finite.length?q(.995):null,canvas:null};
    if(!finite.length)return heatCache;
    const lo=Math.log10(heatCache.lo),span=(Math.log10(heatCache.hi)-lo)||1,lut=LUTS[LAYER_RAMP[l]];
    const off=document.createElement('canvas');off.width=g.columns;off.height=g.rows;const octx=off.getContext('2d'),img=octx.createImageData(g.columns,g.rows),d=img.data;
    for(let i=0;i<n;i++){const v=values[i];if(!(v>0&&Number.isFinite(v)))continue;const k=Math.round(255*Math.max(0,Math.min(1,(Math.log10(v)-lo)/span)))*3;d[i*4]=lut[k];d[i*4+1]=lut[k+1];d[i*4+2]=lut[k+2];d[i*4+3]=255;}
    octx.putImageData(img,0,0);heatCache.canvas=off;return heatCache;
  }
  // Each path of the epoch as a Path2D in world coordinates, built once per snapshot.
  function paths(){
    if(pathCache)return pathCache;
    pathCache=(snapshot?.paths||[]).map(p=>{const path=new Path2D(),pts=p.points;if(pts.length>=2){path.moveTo(pts[0],pts[1]);for(let i=2;i<pts.length;i+=2)path.lineTo(pts[i],pts[i+1]);}return {...p,path2d:path};});
    return pathCache;
  }
  const pathEnd=p=>({x:p.points.at(-2),y:p.points.at(-1)}),pathStart=p=>({x:p.points[0],y:p.points[1]});
  // Truth → estimate error vectors are drawn only for centre errors above VECTOR_MIN units (smaller ones are rounding-level),
  // exaggerated by a 1-2-5 factor (at most ×50) that draws their median about 25 units long.
  const VECTOR_MIN=.5;
  function errorExaggeration(){const m=middle(planetMatches().map(x=>x.distance).filter(d=>d>VECTOR_MIN));if(!Number.isFinite(m))return null;const raw=Math.max(1,Math.min(50,25/m));const p=10**Math.floor(Math.log10(raw)),f=raw/p;return (f>=5?5:f>=2?2:1)*p;}
  function render(){
    const dpr=window.devicePixelRatio||1,rect=canvas.getBoundingClientRect();if(rect.width){canvas.width=Math.round(rect.width*dpr);canvas.height=Math.round(rect.width*H/W*dpr);}const sx=canvas.width/view.width,sy=canvas.height/view.height,px=dpr/sx;ctx.setTransform(sx,0,0,sy,-view.x*sx,-view.y*sy);ctx.fillStyle='#080d13';ctx.fillRect(view.x,view.y,view.width,view.height);
    if(!snapshot){return;}
    const hasDecision=snapshot.selection_kind==='decision';for(const id of ['simulate','compare','audit'])$(id).disabled=!hasDecision;$('decisionPanel').hidden=!hasDecision;
    const g=snapshot.field_grid||{},l=shownLayer(),heat=heatLayer();
    const carried=carriedBanner(l),banner=carried;
    if(heat.canvas){ctx.save();ctx.globalAlpha=carried?.5:.85;ctx.imageSmoothingEnabled=true;ctx.drawImage(heat.canvas,g.origin_x-g.step/2,g.origin_y-g.step/2,g.columns*g.step,g.rows*g.step);ctx.restore();}
    const unit=l==='uncertainty'?'field σ from the fit covariance':l==='error'?'|belief − truth gravity|':'|gravity| of the fitted planets';
    $('scaleMin').textContent=heat.count?fmtSig(heat.lo):'no data';$('scaleMax').textContent=heat.count?fmtSig(heat.hi):'no data';$('scaleUnit').textContent=`${unit} · log scale`;
    $('scaleBar').style.background=`linear-gradient(90deg,${RAMPS[LAYER_RAMP[l]].join(',')})`;
    $('scaleBar').title=`Colours span the 2nd–99.5th percentile on a log scale · ${fmtSig(heat.lo)} to ${fmtSig(heat.hi)} · ${unit}`;
    const overlay=(fn)=>{ctx.save();ctx.setTransform(1,0,0,1,0,0);fn();ctx.restore();};
    if(banner)overlay(()=>{ctx.font=`${13*dpr}px system-ui`;const width=Math.min(canvas.width-16,ctx.measureText(banner).width+20*dpr);ctx.fillStyle='rgba(8,13,19,.82)';ctx.fillRect(8,8,width,26*dpr);ctx.fillStyle='#ffc66d';ctx.fillText(banner,18,26*dpr,width-12);});
    if(!heat.count)overlay(()=>{ctx.fillStyle='#ffc66d';ctx.font=`${14*dpr}px system-ui`;ctx.fillText(l==='uncertainty'?`Fit uncertainty unavailable: ${covarianceNote()||'no covariance in this snapshot'}.`:'No field values for this layer in this snapshot.',16*dpr,28*dpr);});
    if($('pathResiduals').checked){const residuals=(snapshot.paths||[]).flatMap(e=>e.residual_samples||[]),mags=residuals.map(r=>Math.hypot(r[2],r[3])).sort((a,b)=>a-b),cap=mags.length?mags[Math.floor((mags.length-1)*.95)]:null;$('pathScale').style.display=residuals.length?'grid':'none';$('pathMax').textContent=Number.isFinite(cap)?fmt(cap,4):'no data';$('pathScaleBar').title=`Path residual magnitude, colours clip at the 95th percentile (${fmt(cap,6)})`;const lut=LUTS.magma;for(const r of residuals){const k=Math.round(255*Math.max(0,Math.min(1,Math.hypot(r[2],r[3])/(cap||1))))*3;ctx.beginPath();ctx.arc(r[0],r[1],1.7*px,0,Math.PI*2);ctx.fillStyle=`rgb(${lut[k]},${lut[k+1]},${lut[k+2]})`;ctx.fill();}}else $('pathScale').style.display='none';
    if($('paths').checked)drawPaths(px);
    if($('contacts').checked){ctx.fillStyle='#6ef0d0';for(const p of snapshot.diagnostic?.map_belief?.contact_points||[]){ctx.beginPath();ctx.arc(p.x,p.y,2.5*px,0,Math.PI*2);ctx.fill();}}
    drawPlanets(px);
    if($('spawns').checked){
      const spawns=snapshot.diagnostic?.bot_state?.observed_spawn_locations||[];
      ctx.save();ctx.strokeStyle='#9ad7ff';ctx.lineWidth=2*px;const r=8*px;
      for(const p of spawns){ctx.strokeRect(p.x-r,p.y-r,2*r,2*r);ctx.beginPath();ctx.moveTo(p.x-r,p.y);ctx.lineTo(p.x+r,p.y);ctx.moveTo(p.x,p.y-r);ctx.lineTo(p.x,p.y+r);ctx.stroke();}
      ctx.restore();
      if(!spawns.length)overlay(()=>{ctx.fillStyle='#9ad7ff';ctx.font=`${13*dpr}px system-ui`;ctx.fillText('No spawn samples recorded in this snapshot.',16*dpr,canvas.height-16*dpr);});
    }
    for(const [id,list] of Object.entries(snapshot.world?.ghosts||{}))list.forEach((g,k)=>{ctx.beginPath();ctx.arc(g.x,g.y,6*px,0,Math.PI*2);ctx.globalAlpha=.12+.1*k;ctx.fillStyle=colors[Number(id)%colors.length];ctx.fill();ctx.globalAlpha=1;});
    const players=snapshot.world?.players||{};for(const [id,p] of Object.entries(players)){const own=id===String(snapshot.world?.own_id);ctx.beginPath();ctx.arc(p.x,p.y,(own?8:6)*px,0,Math.PI*2);ctx.fillStyle=own?'#f4f7ff':colors[Number(id)%colors.length];ctx.fill();ctx.strokeStyle='#080d13';ctx.lineWidth=2*px;ctx.stroke();ctx.fillStyle='#fff';ctx.font=`${12*px}px system-ui`;ctx.fillText(pname(id),p.x+8*px,p.y-8*px);}
    for(const sim of simulations){const pts=sim.data.points||[];if(!pts.length)continue;ctx.beginPath();ctx.moveTo(pts[0].x,pts[0].y);for(const p of pts)ctx.lineTo(p.x,p.y);ctx.strokeStyle=sim.color;ctx.lineWidth=3*px;ctx.stroke();}
    const shot=sel?.kind==='shot'&&paths().find(p=>p.sequence===sel.sequence);
    if(shot){ctx.strokeStyle='#fff2a8';ctx.lineWidth=3*px;ctx.stroke(shot.path2d);const end=pathEnd(shot);ctx.beginPath();ctx.arc(end.x,end.y,6*px,0,Math.PI*2);ctx.lineWidth=2*px;ctx.stroke();}
    drawReview(px);
    drawDegrade(px);
    drawSelection(px);
    const decision=currentDecision(),choice=decision.choice||{};$('decisionLabel').textContent=snapshot.selection_kind==='decision'?`Decision #${decision.sequence} · ${decision.bot_name} · ${new Date(decision.timestamp_unix_ms||0).toLocaleTimeString()} · belief ${lagText(decision)||(decision.map_updating?'stale (map updating)':decision.map_updating===false?'up to date':'lag not recorded')}`:snapshot.selection_kind==='timeline_event'?`Event #${snapshot.sequence} · belief as of decision snapshot #${snapshot.diagnostic_sequence??'unavailable'}${newerPathsNote()}`:`Latest state · events through #${snapshot.sequence} · belief snapshot #${snapshot.diagnostic_sequence??'unavailable'}${newerPathsNote()}`;
    renderMetrics(decision);renderChoice(choice);renderSelection();renderDegrade();syncLayerButtons();
  }
  // All paths of the map epoch: coloured by inferred outcome, older ones fainter, the selected bot's on top, paths the belief has not seen dashed.
  function drawPaths(px){
    const list=paths(),n=list.length,bot=$('bot').value,botId=bot==='all'?null:[...botNames].find(([,name])=>name===bot)?.[0];
    const order=list.map((p,i)=>({p,i,mine:botId!=null&&String(p.player_id)===botId})).sort((a,b)=>a.mine-b.mine||a.i-b.i);
    ctx.save();ctx.lineCap='round';ctx.lineJoin='round';
    for(const {p,i,mine} of order){
      const age=n>1?i/(n-1):1,alpha=(p.label==='trial'?1:p.label?.startsWith('training')?.35:.15+.6*age)*(botId!=null&&!mine?.35:1);
      ctx.globalAlpha=alpha;ctx.strokeStyle=OUTCOME_COLORS[p.outcome]||OUTCOME_COLORS.unknown;ctx.lineWidth=(p.label==='trial'||mine?2:1.2)*px;
      ctx.setLineDash(newerThanBelief(p)?[5*px,4*px]:[]);ctx.stroke(p.path2d);
    }
    ctx.setLineDash([]);ctx.globalAlpha=1;
    for(const p of list)if(p.label){const s=pathStart(p);ctx.fillStyle='#cfd8e6';ctx.font=`${12*px}px system-ui`;ctx.fillText(p.label,s.x+8*px,s.y+16*px);}
    ctx.restore();
  }
  // Estimated planets as white outlines; truth planets as thin status-coloured outlines with an exaggerated truth → estimate error vector.
  function drawPlanets(px){
    if($('actualPlanets').checked&&snapshot.actual_planets){
      const status=disclosedStatuses(),k=errorExaggeration();
      snapshot.actual_planets.forEach((p,i)=>{const s=status[i]?.status||'ok';ctx.beginPath();ctx.arc(p.x,p.y,Math.max(3*px,p.radius),0,Math.PI*2);ctx.lineWidth=(s==='missing'?1.6:1)*px;ctx.strokeStyle=STATUS_COLORS[s];ctx.setLineDash(s==='missing'?[4*px,3*px]:[]);ctx.stroke();ctx.setLineDash([]);});
      ctx.strokeStyle=ctx.fillStyle='#ff9d4d';ctx.lineWidth=1.5*px;
      for(const m of k?planetMatches():[]){if(!(m.distance>VECTOR_MIN))continue;const dx=(m.estimated.x-m.actual.x)*k,dy=(m.estimated.y-m.actual.y)*k,len=Math.hypot(dx,dy);if(len<2*px)continue;const ex=m.actual.x+dx,ey=m.actual.y+dy,ux=dx/len,uy=dy/len,h=Math.min(6*px,len/2);ctx.beginPath();ctx.moveTo(m.actual.x,m.actual.y);ctx.lineTo(ex,ey);ctx.stroke();ctx.beginPath();ctx.moveTo(ex,ey);ctx.lineTo(ex-ux*h-uy*h*.5,ey-uy*h+ux*h*.5);ctx.lineTo(ex-ux*h+uy*h*.5,ey-uy*h-ux*h*.5);ctx.fill();}
      $('errorScale').textContent=k?`→ centre error (truth to estimate) ×${k}`:'';
    }else $('errorScale').textContent='';
    if($('beliefPlanets').checked){ctx.strokeStyle='#f4f7ff';ctx.lineWidth=1.5*px;for(const p of snapshot.diagnostic?.map_belief?.planets||[]){ctx.beginPath();ctx.arc(p.x,p.y,Math.max(3*px,p.radius||3),0,Math.PI*2);ctx.stroke();}}
  }
  // A path the recorded belief has not seen: it ended after the belief's snapshot (or no belief exists yet).
  function newerPathsNote(){const n=(snapshot.paths||[]).filter(newerThanBelief).length;return n?` · ${n} path${n===1?'':'s'} newer than the belief (dashed)`:'';}
  const newerThanBelief=item=>item.sequence>(snapshot.diagnostic_sequence??-1);
  function fmtSig(v){if(!Number.isFinite(v))return '—';const a=Math.abs(v);return a>=100?v.toFixed(0):a>=1?v.toFixed(2):v.toPrecision(2);}
  function kv(el,rows){el.replaceChildren();for(const [a,b,hint] of rows){if(b===null){const h=document.createElement('b');h.className='wide';h.textContent=a;el.append(h);continue;}const x=document.createElement('span');x.textContent=a;if(hint)x.title=hint;const y=document.createElement('b');y.textContent=b;el.append(x,y);}}
  const SEARCH_ICONS={settled:['✓','#7de3a1'],work_budget:['…','#2aaa8c'],time_budget:['✂','#ff9d4d'],planet_cap:['⊤','#e8d96d']};
  // The belief summary: status pills (lag, search, covariance), one glyph per planet, and two counts. Long explanations live in hovers.
  function renderMetrics(d){
    const pills=$('pills');pills.replaceChildren();
    if(!snapshot.diagnostic){kv($('metrics'),[['No belief recorded yet at this point; the first targeting decision has not happened.',null]]);renderGlyphs();renderUpdate(null);return;}
    const b=snapshot.diagnostic?.map_belief||{},g=snapshot.field_grid||{};
    // Between decisions the belief is the latest decision snapshot, so read its row rather than the (absent) selected decision.
    const source=decisions.find(x=>x.sequence===snapshot.diagnostic_sequence)||d;
    const pill=(text,color,title,dim)=>{const el=document.createElement('span');el.className='pill';el.textContent=text;el.style.borderColor=color;el.style.color=dim?'#6e7d91':color;el.title=title;pills.append(el);};
    const lag=source.lag;pill(lag?(lag.paths?`lag ${lag.inferred?'≈':''}${lag.paths}`:'up to date'):'lag ?',lag?.paths>=2?'#ff5cc8':lag?.paths===1?'#c56aa9':'#7de3a1',`Belief lag: ${lagText(source)||'not recorded in this capture'}. Paths the estimator had queued or was fitting when this belief was read; ≈ means inferred from the recorded inputs.`,!lag);
    const [icon,color]=SEARCH_ICONS[source.search_state]||['?','#8799af'];pill(`search ${icon}`,color,`Planet search: ${searchText(source.search_state)}`,source.search_state==null);
    pill(g.has_covariance?'σ':'σ ✕',g.has_covariance?'#e8b84a':'#8a6d2a',g.has_covariance?'Fit covariance available: the Fit uncertainty layer and per-planet σ use it.':`Fit covariance unavailable: ${covarianceNote()}`,!g.has_covariance);
    kv($('metrics'),[['Accepted contacts',String(b.contact_points?.length??0),'Path endpoints the estimator accepted as planet-surface contacts.'],['Contacts no planet explains',Number.isFinite(b.unmatched_contacts)?String(b.unmatched_contacts):'—','Possible surface contacts farther than 15 units from every estimated planet: reported, never used to create or move a planet.']]);
    renderGlyphs();renderUpdate(source);
  }
  // Error ramp for the planet glyphs: teal (on target) → yellow (at the tolerance) → red (at the match limit), log-scaled.
  const GLYPH_RAMP=['#3fd0c9','#f2d45c','#ff4d6a'];
  function rampColor(err,tol,limit){const t=Math.log1p(Math.max(0,err))/Math.log1p(limit),tt=Math.log1p(tol)/Math.log1p(limit),c=t<=tt?[0,t/tt]:[1,Math.min(1,(t-tt)/(1-tt))];const h=x=>[1,3,5].map(i=>parseInt(x.slice(i,i+2),16)),a=h(GLYPH_RAMP[c[0]]),b2=h(GLYPH_RAMP[c[0]+1]);return `rgb(${a.map((v,i)=>Math.round(v+(b2[i]-v)*c[1])).join(',')})`;}
  // One glyph per planet. With a truth map: one per truth planet in truth order (found: fill = centre error, ring = radius error; missing: dashed red ring), then one per estimate that matches nothing (violet diamond).
  // Without one: one per estimated planet. The count always comes from the capture, never a constant.
  function renderGlyphs(){
    const el=$('glyphs'),note=$('glyphNote');el.replaceChildren();note.textContent='';
    const b=snapshot?.diagnostic?.map_belief,actual=snapshot?.actual_planets,NS='http://www.w3.org/2000/svg';
    const glyph=(draw,title,onclick,selected)=>{const svg=document.createElementNS(NS,'svg');svg.setAttribute('viewBox','0 0 20 20');svg.setAttribute('class',`glyph${selected?' pinned':''}`);const t=document.createElementNS(NS,'title');t.textContent=title;svg.append(t);for(const [tag,attrs] of draw){const e=document.createElementNS(NS,tag);for(const [k,v] of Object.entries(attrs))e.setAttribute(k,v);svg.append(e);}svg.addEventListener('click',onclick);el.append(svg);};
    const select=o=>()=>{sel=o;render();};
    if(actual&&snapshot.planet_match){
      const statuses=disclosedStatuses();
      actual.forEach((p,i)=>{const s=statuses[i],m=s.match,picked=sel?.kind==='actual'&&sel.index===i;
        if(!m){glyph([['circle',{cx:10,cy:10,r:6.5,fill:'none',stroke:STATUS_COLORS.missing,'stroke-width':1.6,'stroke-dasharray':'3 2'}]],`Truth planet #${i} · missing: no estimate within 80 units · r ${fmt(p.radius,1)}`,select({kind:'actual',index:i}),picked);return;}
        glyph([['circle',{cx:10,cy:10,r:6.5,fill:rampColor(m.distance,LOCATION_TOL,80),stroke:rampColor(Math.abs(m.radiusError),SIZE_TOL,40),'stroke-width':2.5}]],`Truth planet #${i} · ${STATUS_LABEL[s.status]} · ${matchNumbers(m)} (estimate #${m.estimatedIndex})`,select({kind:'actual',index:i}),picked);});
      for(const j of snapshot.planet_match.unmatched_estimated||[]){const p=b.planets[j];glyph([['rect',{x:5,y:5,width:10,height:10,transform:'rotate(45 10 10)',fill:'none',stroke:'#c58cff','stroke-width':1.8}]],`Estimated planet #${j} · extra: matches no truth planet within 80 units · (${fmt(p.x,0)}, ${fmt(p.y,0)}) r ${fmt(p.radius,1)}`,select({kind:'planet',index:j}),sel?.kind==='planet'&&sel.index===j);}
      const count=k=>statuses.filter(x=>x.status===k).length,off=count('location')+count('size')+count('both');
      note.textContent=`${b.planets.length} estimated · ${actual.length} truth: ${count('ok')} close, ${off} off, ${count('missing')} missing, ${snapshot.planet_match.unmatched_estimated?.length??0} extra`;
      $('glyphLegend').hidden=false;return;
    }
    $('glyphLegend').hidden=true;
    if(!b){note.textContent='No belief snapshot at this point.';return;}
    (b.planets||[]).forEach((p,i)=>{const s=planetSigma(i);glyph([['circle',{cx:10,cy:10,r:6.5,fill:'#f4f7ff33',stroke:'#f4f7ff','stroke-width':1.5}]],`Estimated planet #${i} · (${fmt(p.x,0)}, ${fmt(p.y,0)}) · r ${fmt(p.radius,1)} · mass ${fmt(p.mass,0)} · position σ ${s?fmt(s.position,1):'unavailable'}`,select({kind:'planet',index:i}),sel?.kind==='planet'&&sel.index===i);});
    note.textContent=`${b.planets?.length??0} estimated${actual?' · the truth map is from a newer map epoch than this belief':' · no truth map in this capture'}`;
  }
  function renderChoice(c){kv($('choice'),[['Shot',c.velocity==null?'none':`v${c.velocity} at ${c.angle_degrees}° · ${c.reason||'shot'}`],['Predicted',c.predicted_victim!=null?`player ${c.predicted_victim} · centre miss ${fmt(c.miss_distance)}`:'no hit'],['Covered path fraction',fmt(c.covered_fraction,3)],...predictiveRows(),...mapAgeRows()]);}
  // Pairs come from the server's single matching (closest first, one-to-one, within the threshold).
  function planetMatches(){
    const m=snapshot?.planet_match,b=snapshot?.diagnostic?.map_belief;if(!m||!b)return [];
    return m.pairs.map(p=>({index:p.actual,estimatedIndex:p.estimated,estimated:b.planets[p.estimated],actual:snapshot.actual_planets[p.actual],distance:p.center_error,radiusError:p.radius_error,massRatio:p.mass_ratio}));
  }
  function matchStatus(m){const loc=m.distance>LOCATION_TOL,size=Math.abs(m.radiusError)>SIZE_TOL;return loc&&size?'both':loc?'location':size?'size':'ok';}
  // One record per truth planet: {status, match}; missing planets have no match.
  function disclosedStatuses(){
    const matches=planetMatches(),out=(snapshot?.actual_planets||[]).map(()=>({status:snapshot.planet_match?'missing':'unknown',match:null}));
    for(const m of matches)out[m.index]={status:matchStatus(m),match:m};
    return out;
  }
  function nearestSurface(point,planets){return planets?.map((p,i)=>({planet:p,index:i,distance:Math.hypot(point.x-p.x,point.y-p.y),residual:Math.abs(Math.hypot(point.x-p.x,point.y-p.y)-p.radius)})).sort((a,b)=>a.residual-b.residual)[0]||null;}
  async function simulate(){const d=currentDecision();if(!d.sequence)return;try{setStatus('Simulating…');const query=`id=${captureId}&seq=${d.sequence}&velocity=${q($('velocity').value)}&angle=${q($('angle').value)}&model=${$('model').value}`;const result=await api(`/api/simulate?${query}`);simulations=[{data:result,color:result.model==='actual'?'#ff8d68':'#8ee5a4'}];setStatus(`${result.termination.kind.replaceAll('_',' ')}${result.termination.player_id!==undefined?` · player ${result.termination.player_id}`:''}`);render();}catch(e){setStatus(e.message);}}
  async function compare(){const d=currentDecision();try{setStatus('Comparing with truth map…');const result=await api(`/api/compare?id=${captureId}&seq=${d.sequence}`);simulations=[];
      const recorded=result.recorded_choice, runs=[];const sim=(choice,model,color,label)=>api(`/api/simulate?id=${captureId}&seq=${d.sequence}&velocity=${choice.velocity}&angle=${choice.angle_degrees}&model=${model}`).then(data=>{simulations.push({data,color,label});runs.push({label,termination:data.termination});});
      if(recorded?.velocity){await sim(recorded,'belief','#8ee5a4','Recorded shot, captured belief');await sim(recorded,'actual','#ff8d68','Recorded shot, truth map');}
      const oracle=result.actual_map_choice_current_policy;if(oracle?.velocity)await sim(oracle,'actual','#ffe27a','Current policy, truth map');
      const choiceText=c=>c?.velocity?`v${c.velocity} @ ${c.angle_degrees}° · ${c.reason||'shot'}${c.predicted_victim!=null?` · victim ${c.predicted_victim}`:''}`:'no shot',endText=r=>`${r.termination.kind.replaceAll('_',' ')}${r.termination.player_id!==undefined?` · player ${r.termination.player_id}`:''}`;kv($('comparison'),[['Recorded choice',choiceText(recorded)],['Actual-map choice (current policy)',choiceText(oracle)],...runs.map(r=>[r.label,endText(r)]),...(result.note?[[result.note,null]]:[])]);
      setStatus('Green: belief · orange: recorded shot on actual map · yellow: current policy on actual map');render();
    }catch(e){$('comparison').textContent=e.message;setStatus(e.message);}}
  async function audit(){const d=currentDecision();try{setStatus('Replaying observed paths…');const result=await api(`/api/physics-audit?id=${captureId}&seq=${d.sequence}`);simulations=[{data:{points:result.worst_observed_points},color:'#d7e2ff'},{data:{points:result.worst_rust_points},color:'#ff4da6'}];kv($('comparison'),[['Audited shots',String(result.audited_shots)],['Planets in map',String(result.map_planets)],['Timed samples',String(result.timed_samples)],['Coordinate RMSE',fmt(result.coordinate_rmse)],['Median / p95 / max position error',`${fmt(result.median_position_error)} / ${fmt(result.p95_position_error)} / ${fmt(result.max_position_error)}`],['Worst shot',(w=>w?`event #${w.shot_sequence} · player ${w.player_id} · v${fmt(w.velocity,0)} @ ${fmt(w.angle,1)}° · RMSE ${fmt(w.coordinate_rmse)} · max ${fmt(w.max_position_error)} · Rust: ${String(w.rust_termination?.kind||'—').replaceAll('_',' ')}`:'—')(result.worst_shot)],...(result.note?[[result.note,null]]:[])]);setStatus(`Rust path replay: coordinate RMSE ${fmt(result.coordinate_rmse)} units · worst shot overlaid`);render();}catch(e){$('comparison').textContent=e.message;setStatus(e.message);}}
  $('capture').addEventListener('change',()=>{showCaptureView();resetView();loadDecisions();});$('bot').addEventListener('change',()=>{drawScrub();render();});$('refresh').addEventListener('click',()=>{if(!$('overview').hidden)loadOverview();else loadDecisions($('bot').value);});for(const id of ['paths','beliefPlanets','actualPlanets','contacts','spawns'])$(id).addEventListener('change',render);$('pathResiduals').addEventListener('change',loadSnapshot);$('simulate').addEventListener('click',simulate);$('compare').addEventListener('click',compare);$('audit').addEventListener('click',audit);window.addEventListener('resize',()=>{render();drawScrub();});
  let viewportTimer=null;function refreshViewport(){clearTimeout(viewportTimer);viewportTimer=setTimeout(loadSnapshot,120);}
  function zoomAt(factor,px=W/2,py=H/2){const width=Math.max(40,Math.min(W,view.width*factor)),height=width*H/W,cx=view.x+px/W*view.width,cy=view.y+py/H*view.height;view={x:Math.max(0,Math.min(W-width,cx-px/W*width)),y:Math.max(0,Math.min(H-height,cy-py/H*height)),width,height};render();refreshViewport();}
  function resetView(){view={x:0,y:0,width:W,height:H};loadSnapshot();}
  $('resetView').addEventListener('click',resetView);
  // Wheel zooms about the cursor.
  canvas.addEventListener('wheel',e=>{e.preventDefault();const rect=canvas.getBoundingClientRect();zoomAt(Math.exp(e.deltaY*.0015),(e.clientX-rect.left)/rect.width*W,(e.clientY-rect.top)/rect.height*H);},{passive:false});
  canvas.addEventListener('dblclick',resetView);
  canvas.addEventListener('pointerdown',e=>{panned=false;if(!e.shiftKey&&e.button!==1)return;const rect=canvas.getBoundingClientRect();viewDrag={clientX:e.clientX,clientY:e.clientY,rect,view:{...view}};canvas.setPointerCapture(e.pointerId);});
  canvas.addEventListener('pointermove',e=>{if(!viewDrag)return;const dx=(e.clientX-viewDrag.clientX)/viewDrag.rect.width*viewDrag.view.width,dy=(e.clientY-viewDrag.clientY)/viewDrag.rect.height*viewDrag.view.height;view={...viewDrag.view,x:Math.max(0,Math.min(W-viewDrag.view.width,viewDrag.view.x-dx)),y:Math.max(0,Math.min(H-viewDrag.view.height,viewDrag.view.y-dy))};render();});canvas.addEventListener('pointerup',()=>{if(viewDrag){viewDrag=null;panned=true;refreshViewport();}});
  // ---- Captures overview: the landing table, one row per capture, sortable, with global metrics ----
  let overviewRows=[],overviewSort={key:'id',dir:-1};
  const overviewValue={
    id:r=>r.id,kind:r=>r.kind,name:r=>r.name,map:r=>r.session?.map_index,decisions:r=>r.decisions,shots:r=>r.shots,
    estimated:r=>r.last?.planets,disclosed:r=>r.last?.accuracy?.actual_planets,missing:r=>r.last?.accuracy?.missing_planets,
    missingShare:r=>r.last?.accuracy?r.last.accuracy.missing_planets/Math.max(1,r.last.accuracy.actual_planets):null,
    centerError:r=>r.last?.accuracy?.median_center_error,search:r=>r.last?.search_state,
  };
  const overviewColumns=[['id','#'],['kind','kind'],['name','name'],['map','map'],['decisions','decisions'],['shots','shots'],['estimated','est. planets'],['disclosed','truth'],['missing','missing'],['missingShare','missing %'],['centerError','median center err'],['search','search']];
  function middle(values){const a=values.filter(Number.isFinite).sort((x,y)=>x-y);if(!a.length)return null;const m=a.length>>1;return a.length%2?a[m]:(a[m-1]+a[m])/2;}
  async function loadOverview(){try{overviewRows=(await api('/api/overview')).captures;renderOverview();}catch(e){setStatus(e.message);}}
  function renderOverview(){
    const kind=$('overviewKind').value,rows=overviewRows.filter(r=>kind==='history'?r.kind==='scenario':r.latest_revision&&(kind==='all'||r.kind===kind));
    const sortValue=overviewValue[overviewSort.key];
    rows.sort((a,b)=>{const x=sortValue(a),y=sortValue(b);if(x==null&&y==null)return 0;if(x==null)return 1;if(y==null)return -1;return (x<y?-1:x>y?1:0)*overviewSort.dir;});
    const withTruth=rows.filter(r=>r.last?.accuracy);
    const pct=v=>Number.isFinite(v)?`${fmt(v*100,0)}%`:'—',units=v=>Number.isFinite(v)?`${fmt(v)} units`:'—';
    kv($('globalMetrics'),[
      ['Captures shown',`${rows.length} · ${rows.filter(r=>r.kind==='live').length} live, ${rows.filter(r=>r.kind==='scenario').length} scenario`],
      ['Shots · decisions',`${rows.reduce((s,r)=>s+r.shots,0)} · ${rows.reduce((s,r)=>s+r.decisions,0)}`],
      ['With a truth map and a decision',String(withTruth.length)],
      ['Median final missing share',pct(middle(withTruth.map(overviewValue.missingShare)))],
      ['Median final center error',units(middle(withTruth.map(overviewValue.centerError)))],
    ]);
    const table=$('overviewTable');table.replaceChildren();
    const head=document.createElement('tr');
    for(const [key,label] of overviewColumns){const th=document.createElement('th');th.textContent=label+(overviewSort.key===key?(overviewSort.dir>0?' ▲':' ▼'):'');th.addEventListener('click',()=>{overviewSort={key,dir:overviewSort.key===key?-overviewSort.dir:-1};renderOverview();});head.append(th);}
    table.append(head);
    for(const r of rows){
      const a=r.last?.accuracy,tr=document.createElement('tr');tr.className='row';
      tr.title=`${new Date(r.started_at_unix_ms).toLocaleString()}${r.session?` · session ${r.session.id}`:''}${r.revision?` · ${r.revision.slice(0,8)}`:''}${r.last?.stage?` · ${r.last.stage}`:''}`;
      const cells=[r.id,r.kind,r.name,r.session?.map_index??'—',r.decisions,r.shots,r.last?.planets??'—',a?.actual_planets??'—',a?.missing_planets??'—',a?pct(a.missing_planets/Math.max(1,a.actual_planets)):'—',a&&a.median_center_error!=null?fmt(a.median_center_error,1):'—',r.last?.search_state?.replaceAll('_',' ')??'—'];
      for(const c of cells){const td=document.createElement('td');td.textContent=String(c);tr.append(td);}
      tr.addEventListener('click',()=>openCapture(r.id));table.append(tr);
    }
  }
  async function openCapture(id){
    if(![...$('capture').options].some(o=>o.value===String(id)))await fillCaptureSelect();
    $('capture').value=String(id);showCaptureView();resetView();loadDecisions();
  }
  function showCaptureView(){$('overview').hidden=true;document.querySelector('main').hidden=false;}
  $('overviewBtn').addEventListener('click',()=>{if($('overview').hidden){$('overview').hidden=false;document.querySelector('main').hidden=true;loadOverview();}else if(captureId!==null){showCaptureView();render();drawScrub();}});
  $('overviewKind').addEventListener('change',renderOverview);

  // ---- Shot review: what the recorded belief predicted for the chosen shot versus what happened ----
  let review=null,reviewRequest=0,reviewVisible=new Set(),predictive=new Map(),predictiveHorizon=5;
  const REVIEW_COLORS={observed:'#ffffff',physical:'#9aa7ff',actual:'#ff8d68'};
  function terminationText(t){
    switch(t?.kind){
      case 'planet_collision':return `planet #${t.planet_index}`;
      case 'player_collision':return `player ${pname(t.player_id)}`;
      case 'battlefield_margin':return 'left the battlefield';
      case 'sample_limit':return 'sample limit';
      default:return '—';
    }
  }
  function outcomeText(o){
    switch(o?.kind){
      case 'player':return `player ${pname(o.player_id)} (${fmt(o.distance,1)} units)`;
      case 'planet':return `${o.source} planet #${o.index} surface`;
      case 'battlefield_margin':return 'left the battlefield';
      case 'sample_limit':return 'sample limit';
      default:return 'free space (unexplained)';
    }
  }
  async function loadReview(repairs=false){
    const request=++reviewRequest,id=captureId,seq=position;
    if(snapshot?.selection_kind!=='decision'){review=null;renderReview();return;}
    if(!repairs){review={loading:true};renderReview();render();}
    try{
      const data=await api(`/api/shot-review?id=${id}&seq=${seq}${repairs?'&repairs=1':''}`);
      if(request!==reviewRequest||id!==captureId||seq!==position)return;
      if(!repairs||!review)reviewVisible=new Set(['observed',...(data.variants||[]).filter(v=>v.as_used||v.key==='actual').map(v=>v.key)]);
      review=data;renderReview();render();
    }catch(e){if(request===reviewRequest){review={error:e.message};renderReview();}}
  }
  function renderReview(){
    const summary=$('reviewSummary'),paths=$('reviewPaths'),note=$('reviewNote'),repairs=$('reviewRepairs');
    paths.replaceChildren();note.textContent='';repairs.replaceChildren();
    if(!review){kv(summary,[['Select a decision to review its shot.',null]]);return;}
    if(review.loading){kv(summary,[['Loading shot review…',null]]);return;}
    if(review.error){kv(summary,[[review.error,null]]);return;}
    const d=review.decision,chosen=d.velocity==null?'no shot chosen':`v${d.velocity} at ${d.angle_degrees}° by player ${pname(d.own_id)}`;
    const predicted=d.velocity==null?'—':`${d.reason||'shot'}${d.predicted_victim!=null?` on player ${d.predicted_victim}`:''} · center miss ${fmt(d.miss_distance)} units`;
    if(!review.shot){kv(summary,[['Chosen shot',chosen],['Predicted',predicted],[review.reason,null]]);return;}
    const s=review.shot;
    kv(summary,[['Chosen shot',chosen],['Predicted by the bot',predicted],['Observed (inferred)',outcomeText(s.outcome)],['Observed path',`events #${s.begin_sequence} → #${s.end_sequence} · ${s.points.length} samples`],...(s.launch_offset>1?[['Launch moved after the decision',`${fmt(s.launch_offset,1)} units`]]:[])]);
    const table=document.createElement('table');table.className='table';
    const row=(cells,cls)=>{const tr=document.createElement('tr');if(cls)tr.className=cls;for(const c of cells){const td=document.createElement(cls==='head'?'th':'td');if(c instanceof Node)td.append(c);else td.textContent=c;tr.append(td);}return tr;};
    const toggle=(key,color)=>{const label=document.createElement('label');label.title='Show on the map';const box=document.createElement('input');box.type='checkbox';box.checked=reviewVisible.has(key);box.addEventListener('change',()=>{box.checked?reviewVisible.add(key):reviewVisible.delete(key);render();});const sw=document.createElement('i');sw.className='dot';sw.style.background=color;sw.style.marginLeft='4px';label.append(box,sw);return label;};
    table.append(row(['','path','RMSE','parts at','ends'],'head'));
    table.append(row([toggle('observed',REVIEW_COLORS.observed),'Observed','—','—',outcomeText(s.outcome)]));
    for(const v of review.variants){
      if(!v.available){const tr=row(['',`${v.label} (not recorded)`,'—','—','—']);tr.classList.add('off');table.append(tr);continue;}
      table.append(row([toggle(v.key,REVIEW_COLORS[v.key]),`${v.label}${v.as_used?' ★':''}`,fmt(v.rmse,1),v.diverges_at==null?'never':`sample ${v.diverges_at}`,terminationText(v.termination)]));
    }
    paths.append(table);
    const div=review.divergence,est=div?.nearest_estimated,act=div?.nearest_disclosed;
    note.textContent=`★ is the model the bot used. RMSE is the per-coordinate path error in world units over the timed samples; a path that stops earlier than the other is held at its last point, so an early stop counts as error. “parts at” is the first sample more than 10 units from the observed path. ${div?`The model used leaves the path at (${fmt(div.point.x,0)}, ${fmt(div.point.y,0)}); the nearest estimated planet is #${est?.index} (${fmt(est?.gap,0)} from its surface)${act?`, the nearest truth planet #${act.index} (${fmt(act.gap,0)})`:''}. Distance is not cause: a far planet can bend a path.`:''}`;
    if(snapshot?.actual_planets){
      if(review.repairs?.rows){
        const r=review.repairs,t=document.createElement('table');t.className='table';
        t.append(row(['truth planet','change','RMSE after','improves by'],'head'));
        const status=disclosedStatuses();
        for(const x of r.rows.slice(0,8))t.append(row([`#${x.disclosed_index} (${STATUS_LABEL[status[x.disclosed_index]?.status]||'—'})`,x.kind==='add_missing'?'add to belief':`correct estimate #${x.estimated_index}`,fmt(x.rmse,1),sgn(x.improvement)]));
        const n=document.createElement('div');n.className='note';n.textContent=`Baseline RMSE ${fmt(r.baseline_rmse,1)}; correcting every listed planet at once gives ${fmt(r.all_repaired_rmse,1)}. ${r.note}`;
        repairs.append(t,n);
      }else if(review.repairs?.error){repairs.textContent=review.repairs.error;}
      else{const b=document.createElement('button');b.textContent='Test repairs with the truth map';b.title='Change one planet of the belief to the truth one, re-simulate, and rank by how much the path improves';b.addEventListener('click',()=>{b.disabled=true;b.textContent='Simulating…';loadReview(true);});repairs.append(b);}
    }
  }
  function drawReview(px){
    if(!review?.shot||snapshot?.selection_kind!=='decision'||String(review.decision.sequence)!==position)return;
    const line=(points,color,width,dash)=>{if(!points?.length)return;ctx.beginPath();ctx.moveTo(points[0].x,points[0].y);for(const p of points)ctx.lineTo(p.x,p.y);ctx.strokeStyle=color;ctx.lineWidth=width*px;ctx.setLineDash(dash.map(d=>d*px));ctx.stroke();ctx.setLineDash([]);};
    for(const v of review.variants)if(v.available&&reviewVisible.has(v.key))line(v.points,REVIEW_COLORS[v.key],v.as_used?3:2,[7,5]);
    // The decision's own shot: bold, on top of every other path.
    if(reviewVisible.has('observed')){line(review.shot.points,'#080d13',5.5,[]);line(review.shot.points,REVIEW_COLORS.observed,3,[]);}
    const div=review.divergence;if(div&&reviewVisible.has((review.variants.find(v=>v.as_used)||{}).key)){ctx.beginPath();ctx.arc(div.point.x,div.point.y,9*px,0,Math.PI*2);ctx.strokeStyle='#ffe27a';ctx.lineWidth=2.5*px;ctx.stroke();}
  }
  // ---- Degradation experiment: damage the truth map, let the policy choose, fly the shot on the truth ----
  let degrade=null,degradeRequest=0;
  async function runDegrade(){
    const request=++degradeRequest,id=captureId,seq=position,kind=$('degradeKind').value;
    degrade={loading:true};renderDegrade();
    try{
      const data=await api(`/api/degrade?id=${id}&seq=${seq}&kind=${kind}`);
      if(request!==degradeRequest||id!==captureId||seq!==position)return;
      degrade={...data,selected:null};render();
    }catch(e){if(request===degradeRequest){degrade={error:e.message};renderDegrade();}}
  }
  function renderDegrade(){
    const panel=$('degradePanel'),out=$('degradeResult');
    panel.hidden=!(snapshot?.selection_kind==='decision'&&snapshot.actual_planets);
    out.replaceChildren();$('degradeRun').disabled=Boolean(degrade?.loading)||String(snapshot?.sequence)!==position;
    if(!degrade)return;
    if(degrade.loading){out.textContent='Running the policy on each damaged map…';return;}
    if(degrade.error){out.textContent=degrade.error;return;}
    const rows=degrade.rows,damaged=rows.slice(1).filter(r=>!r.error);
    const claimedHit=damaged.filter(r=>r.claimed?.predicted_hit).length,reallyHit=damaged.filter(r=>r.real?.hit_player!=null).length,perfectStillHits=damaged.filter(r=>r.perfect_shot_on_damaged_map?.hit).length;
    const summary=document.createElement('div');summary.className='note';
    summary.textContent=`${damaged.length} damaged maps: the bot claimed a hit on ${claimedHit} and really hit a player on ${reallyHit}; the perfect-information shot would still hit on ${perfectStillHits}.`;
    const table=document.createElement('table');table.className='table';
    const row=(cells,cls,onclick)=>{const tr=document.createElement('tr');if(cls)tr.className=cls;for(const c of cells){const td=document.createElement(cls==='head'?'th':'td');td.textContent=c;tr.append(td);}if(onclick)tr.addEventListener('click',onclick);return tr;};
    table.append(row(['damage','perfect shot if map were real','bot chose (on damaged map)','on the true map'],'head'));
    rows.forEach((r,i)=>{
      if(r.error){table.append(row([r.label,r.error,'','']));return;}
      const f=r.perfect_shot_on_damaged_map,c=r.claimed,re=r.real;
      const fixed=f.hit?'still hits':`closest approach ${fmt(f.closest_approach,1)}`;
      const chose=!c?'no shot':`${c.reason} v${c.velocity}@${c.angle_degrees}°${c.predicted_victim!=null?` → ${c.predicted_victim}`:''}`;
      const real=!re?'—':re.hit_intended?'hits':re.hit_player!=null?`hits player ${pname(re.hit_player)}${c?.predicted_victim==null?' (the bot expected no hit)':' (not the intended target)'}`:`${re.closest_approach==null?'no target':`closest approach ${fmt(re.closest_approach,1)}`} (${terminationText(re.termination)})`;
      table.append(row([r.label,fixed,chose,real],`row${degrade.selected===i?' pinned':''}`,()=>{degrade.selected=i;renderDegrade();render();}));
    });
    const note=document.createElement('div');note.className='note';note.textContent=`${degrade.note} Distances are center to center (a player's radius is 4). Click a row to draw it: orange dashed is the path the bot expected, blue is what happens on the true map.`;
    out.append(summary,table,note);
  }
  function drawDegrade(px){
    const r=degrade?.rows?.[degrade.selected];if(!r?.claimed||snapshot?.selection_kind!=='decision')return;
    const line=(points,color,dash)=>{if(!points?.length)return;ctx.beginPath();ctx.moveTo(points[0].x,points[0].y);for(const p of points)ctx.lineTo(p.x,p.y);ctx.strokeStyle=color;ctx.lineWidth=3*px;ctx.setLineDash(dash.map(d=>d*px));ctx.stroke();ctx.setLineDash([]);};
    line(r.claimed_points,'#ffb45c',[7,5]);line(r.real_points,'#6ec8ff',[]);
  }
  $('degradeRun').addEventListener('click',runDegrade);
  // Held-out belief quality: each decision's belief scored on the next observed shots (needs no truth map).
  async function loadPredictive(){
    const id=captureId;predictive=new Map();
    try{const data=await api(`/api/predictive?id=${id}`);if(id!==captureId)return;predictive=new Map(data.decisions.map(d=>[d.sequence,d]));predictiveHorizon=data.horizon_shots;drawScrub();if(snapshot)render();}catch(e){/* the strip simply lacks the line */}
  }
  function predictiveRows(){
    const p=predictive.get(selected());if(!p)return [];
    const why={no_belief:'no belief recorded yet',no_later_shots:'no observed shots after this decision in the same map epoch',model_unavailable:'the model this decision used is not recorded',too_short_to_score:'observed shots too short to score'};
    return [[`Next ${predictiveHorizon} observed shots`,p.status==='scored'?`median path error ${fmt(p.median_rmse,1)} · worst ${fmt(p.max_rmse,1)} (${p.shots} shots)`:(why[p.status]||'unavailable')]];
  }

  $('worstPrediction').addEventListener('click',()=>{
    let worst=null;for(const [sequence,p] of predictive)if(p.shots>=1&&Number.isFinite(p.median_rmse)&&botVisible(decisionAt(sequence))&&(!worst||p.median_rmse>worst.median_rmse))worst={sequence,median_rmse:p.median_rmse};
    if(!worst)return;const i=timeline.findIndex(item=>item.sequence===worst.sequence);if(i>=0)goto(i);
  });

  // ---- Layers: Gravity, Uncertainty and Error are self-contained modes; the client derives every vector from the per-sample fields ----
  function syncLayerButtons(){
    const hasOracle=Boolean(snapshot?.actual_planets),shown=shownLayer();
    const missing={
      uncertainty:snapshot?.field_grid?.sigma?null:covarianceNote()||'no covariance in this snapshot',
      error:hasOracle?null:'needs a truth map; this capture has none at this point'};
    for(const b of document.querySelectorAll('.layer')){
      const name=b.dataset.layer;if(b.dataset.help===undefined)b.dataset.help=b.title;
      b.classList.toggle('active',name===shown);b.classList.toggle('unavailable',Boolean(missing[name]));
      b.title=missing[name]?`Unavailable: ${missing[name]}`:snapshot?.field_grid?.carried?.[name]?`${b.dataset.help} (showing decision #${snapshot.field_grid.carried[name]}: the current decision has none)`:b.dataset.help;
    }
    $('errorLayer').disabled=!hasOracle;
    $('actualPlanetsLabel').style.visibility=$('actualLegend').style.visibility=hasOracle?'':'hidden';
  }
  for(const b of document.querySelectorAll('.layer'))b.addEventListener('click',()=>{layer=b.dataset.layer;render();});
  syncLayerButtons();
  // ---- Time navigation: one scrubber over every event and decision in the capture ----
  // Rows of the timeline canvas (CSS pixels): two trend panels with their own axes, event ticks, then per-decision strips.
  const scrub=$('scrub'),scrubCtx=scrub.getContext('2d'),SCRUB_H=160,PAD_L=40,PAD_R=8,PANEL_A=[4,40],PANEL_B=[46,80],TICKS=[86,112],LAG_Y=115,COV_Y=120,WORK=[125,158];
  let scrubDrag=false;
  const decisionAt=seq=>decisions.find(d=>d.sequence===seq);
  const botVisible=d=>$('bot').value==='all'||d?.bot_name===$('bot').value;
  const positionFor=item=>item.kind==='decision'&&decisionAt(item.sequence)?String(item.sequence):`event:${item.sequence}`;
  function headIndex(){
    if(position==='none')return timeline.length-1;
    const seq=Number(position.replace('event:',''));
    return timeline.findIndex(item=>item.sequence===seq);
  }
  const scrubX=(i,w)=>PAD_L+(i+.5)/Math.max(1,timeline.length)*(w-PAD_L-PAD_R);
  function scrubIndexAt(clientX){
    const rect=scrub.getBoundingClientRect(),w=rect.width;
    return Math.max(0,Math.min(timeline.length-1,Math.floor((clientX-rect.left-PAD_L)/(w-PAD_L-PAD_R)*timeline.length)));
  }
  function goto(target){
    position=target==='none'?'none':positionFor(timeline[target]);
    drawScrub();refreshViewport();
  }
  function stepEvent(dir){if(!timeline.length)return;const i=headIndex()+dir;if(i>=0&&i<timeline.length)goto(i);}
  function stepDecision(dir){
    for(let i=headIndex()+dir;i>=0&&i<timeline.length;i+=dir)
      if(timeline[i].kind==='decision'&&botVisible(decisionAt(timeline[i].sequence))){goto(i);return;}
  }
  function drawScrub(){
    const dpr=window.devicePixelRatio||1,rect=scrub.getBoundingClientRect(),w=rect.width;
    scrub.width=Math.round(w*dpr);scrub.height=Math.round(SCRUB_H*dpr);
    const c=scrubCtx;c.setTransform(dpr,0,0,dpr,0,0);c.fillStyle='#080d13';c.fillRect(0,0,w,SCRUB_H);
    if(!timeline.length){c.fillStyle='#8799af';c.font='12px system-ui';c.fillText('No events in this capture',PAD_L,SCRUB_H/2);return;}
    const bots=[...new Set(decisions.map(d=>d.bot_name))];
    const points=timeline.map((item,i)=>({item,i,d:item.kind==='decision'?decisionAt(item.sequence):null})).filter(p=>p.d&&botVisible(p.d));
    c.font='10px ui-monospace,monospace';
    const panel=([top,bottom],title)=>{c.fillStyle='#0d1520';c.fillRect(PAD_L,top,w-PAD_L-PAD_R,bottom-top);let x=PAD_L+4;for(const [text,color] of title){c.fillStyle=color;c.fillText(text,x,top+10);x+=c.measureText(text).width+10;}};
    const axisLabel=(text,y)=>{c.fillStyle='#7d8ea5';c.textAlign='right';c.fillText(text,PAD_L-4,y+3);c.textAlign='left';};
    const series=(list,value,y,color,steps)=>{c.strokeStyle=color;c.lineWidth=1.5;c.beginPath();let pen=false,last=null;for(const p of list){const v=value(p);if(!Number.isFinite(v)){pen=false;continue;}const x=scrubX(p.i,w),yy=y(v);if(pen&&steps)c.lineTo(x,last);pen?c.lineTo(x,yy):c.moveTo(x,yy);pen=true;last=yy;}c.stroke();};
    // Panel A: map quality, linear count axis shared by estimated planets and missing truth planets.
    const withTruth=points.filter(p=>p.d.accuracy),maxPlanets=Math.max(1,...points.map(p=>Math.max(p.d.map_planets||0,p.d.accuracy?.actual_planets||0)));
    const yA=v=>PANEL_A[1]-2-v/maxPlanets*(PANEL_A[1]-PANEL_A[0]-14);
    panel(PANEL_A,[['planets: estimated','#ffc45b'],...(withTruth.length?[['missing truth','#ff4d6a']]:[])]);axisLabel(String(maxPlanets),yA(maxPlanets));axisLabel('0',yA(0));
    series(points,p=>p.d.map_planets||0,yA,'#ffc45b',true);
    if(withTruth.length)series(withTruth,p=>p.d.accuracy.missing_planets,yA,'#ff4d6a',true);
    // Panel B: prediction, next-shot median path error on a log axis with decade gridlines.
    const scored=points.filter(p=>predictive.get(p.d.sequence)?.median_rmse>0),pv=p=>predictive.get(p.d.sequence).median_rmse;
    panel(PANEL_B,[[`next-${predictiveHorizon}-shot median path error (log)`,'#7de3a1']]);
    if(scored.length){
      const vals=scored.map(pv),lo=Math.floor(Math.log10(Math.min(...vals))),hi=Math.max(lo+1,Math.ceil(Math.log10(Math.max(...vals))));
      const yB=v=>PANEL_B[1]-2-(Math.log10(v)-lo)/(hi-lo)*(PANEL_B[1]-PANEL_B[0]-14);
      for(let k=lo;k<=hi;k++){const y=yB(10**k);c.strokeStyle='#1c2735';c.lineWidth=1;c.beginPath();c.moveTo(PAD_L,y);c.lineTo(w-PAD_R,y);c.stroke();if(k===lo||k===hi)axisLabel(10**k>=1000?`${10**(k-3)}k`:String(10**k),y);}
      series(scored,pv,yB,'#7de3a1',false);
      c.fillStyle='#7de3a1';for(const p of scored){c.beginPath();c.arc(scrubX(p.i,w),yB(pv(p)),1.8,0,Math.PI*2);c.fill();}
    }else{c.fillStyle='#6e7d91';c.fillText('no scored decisions',PAD_L+4,PANEL_B[1]-6);}
    // Event ticks: decisions tall and coloured by bot, shot ends medium, map resets red across the trends, everything else faint.
    for(let i=0;i<timeline.length;i++){
      const item=timeline[i],x=scrubX(i,w);let top=TICKS[0]+16,color='#2c3a4d';
      if(item.kind==='decision'){const d=decisionAt(item.sequence);top=TICKS[0];color=colors[Math.max(0,bots.indexOf(d?.bot_name))%colors.length]+(botVisible(d)?'':'44');}
      else if(item.kind==='shot_fin'){top=TICKS[0]+8;color='#7a8ea8';}
      else if(item.kind==='planets'){top=PANEL_A[0];color='#ff6b6b';}
      c.strokeStyle=color;c.lineWidth=item.kind==='decision'||item.kind==='planets'?2:1;c.beginPath();c.moveTo(x,top);c.lineTo(x,TICKS[1]);c.stroke();
    }
    // Per-decision strips: belief lag, covariance, and a mini stacked column of the update's phase wall time (sqrt height; red cap: a part exceeded its ceiling; orange cap: time cut).
    axisLabel('lag σ',(LAG_Y+COV_Y)/2+1);axisLabel('update',(WORK[0]+WORK[1])/2);
    const maxPhase=Math.max(1e-9,...points.map(p=>p.d.update?.phase_ms||0)),workH=WORK[1]-WORK[0]-3;
    for(const p of points){
      const d=p.d,x=scrubX(p.i,w),lag=d.lag?.paths;
      c.fillStyle=lag>=2?'#ff5cc8':lag===1?'#8a3a76':'#1c2735';c.fillRect(x-1,LAG_Y-1,2,4);
      c.fillStyle=d.covariance?'#e8b84a':d.covariance_reason==='not_computed'?'#8a6d2a':'#1c2735';c.fillRect(x-1,COV_Y-1,2,4);
      const u=d.update;if(!u?.phase_ms)continue;
      const height=Math.sqrt(u.phase_ms/maxPhase)*workH;let y=WORK[1];
      for(const part of u.parts){if(!part.phase||!(part.total_ms>0))continue;const h=height*part.total_ms/u.phase_ms;c.fillStyle=PART_COLORS[part.part]||'#8799af';c.fillRect(x-1,y-h,2,h);y-=h;}
      if(u.violations?.length||d.search_state==='time_budget'){c.fillStyle=u.violations?.length?'#ff2d55':'#ff9d4d';c.fillRect(x-1.5,y-3,3,3);}
    }
    const h=headIndex();if(h>=0){const x=scrubX(h,w);c.strokeStyle='#ffe27a';c.lineWidth=2;c.beginPath();c.moveTo(x,0);c.lineTo(x,SCRUB_H);c.stroke();c.fillStyle='#ffe27a';c.beginPath();c.moveTo(x-5,0);c.lineTo(x+5,0);c.lineTo(x,7);c.fill();}
  }
  function scrubTip(i){
    const item=timeline[i],d=item.kind==='decision'?decisionAt(item.sequence):null;
    const lines=[`#${item.sequence} ${item.kind} · ${item.detail.replace(/player (\d+)/g,(m,id)=>`player ${pname(id)}`)}`,new Date(item.timestamp_unix_ms).toLocaleTimeString()];
    if(d)lines.push(`${d.bot_name} · ${d.map_planets} planets`,`fit covariance: ${d.covariance?'recorded':(COVARIANCE_REASONS[d.covariance_reason]||'not recorded')}`,`planet search: ${searchText(d.search_state)}`,`belief: ${lagText(d)||'lag not recorded'}`);
    if(d?.update)lines.push(`update: ${d.update.evaluations} evaluations · phases ${fmt(d.update.phase_ms,0)} ms (${d.update.parts.filter(p=>p.phase&&p.total_ms>0).map(p=>`${PART_LABELS[p.part]||p.part} ${fmt(p.total_ms,0)}`).join(', ')})${d.update.violations?.length?` · ceiling exceeded: ${d.update.violations.map(v=>`${PART_LABELS[v.part]||v.part} ${fmt(v.observed_ms,0)}/${fmt(v.ceiling_ms,0)} ms`).join(', ')}`:''}`);
    const pr=d&&predictive.get(d.sequence);if(pr?.shots)lines.push(`next ${predictiveHorizon} shots: median path error ${fmt(pr.median_rmse,1)} (${pr.shots} observed)`);
    if(d?.accuracy)lines.push(`${d.accuracy.missing_planets} of ${d.accuracy.actual_planets} truth planets missing · median center error ${fmt(d.accuracy.median_center_error,1)}`);
    return lines;
  }
  scrub.addEventListener('pointerdown',e=>{if(!timeline.length)return;scrubDrag=true;scrub.setPointerCapture(e.pointerId);goto(scrubIndexAt(e.clientX));});
  scrub.addEventListener('pointermove',e=>{
    if(!timeline.length)return;const i=scrubIndexAt(e.clientX);
    if(scrubDrag&&i!==headIndex())goto(i);
    tip.replaceChildren(...scrubTip(i).map((t,k)=>{const d=document.createElement('div');if(!k)d.style.fontWeight='600';d.textContent=t;return d;}));
    tip.style.display='block';tip.style.left=`${Math.min(e.clientX+14,window.innerWidth-340)}px`;tip.style.top=`${e.clientY+16}px`;
  });
  for(const type of ['pointerup','pointercancel','lostpointercapture'])scrub.addEventListener(type,()=>{scrubDrag=false;});
  scrub.addEventListener('mouseleave',()=>{tip.style.display='none';});
  $('prevDecision').addEventListener('click',()=>stepDecision(-1));$('nextDecision').addEventListener('click',()=>stepDecision(1));
  $('prevEvent').addEventListener('click',()=>stepEvent(-1));$('nextEvent').addEventListener('click',()=>stepEvent(1));
  $('latest').addEventListener('click',()=>{position='none';drawScrub();loadSnapshot();});
  document.addEventListener('keydown',e=>{
    if(!$('overview').hidden||['INPUT','SELECT','TEXTAREA'].includes(document.activeElement?.tagName)||e.metaKey||e.ctrlKey||e.altKey)return;
    if(e.key==='ArrowLeft'||e.key==='ArrowRight'){e.preventDefault();(e.shiftKey?stepDecision:stepEvent)(e.key==='ArrowLeft'?-1:1);}
    else if(e.key==='Home'&&timeline.length){e.preventDefault();goto(0);}
    else if(e.key==='End'){e.preventDefault();position='none';drawScrub();loadSnapshot();}
  });
  // ---- Object inspection: hit testing, tooltip, pinned selection, planet list ----
  const tip=$('tip');
  const sgn=v=>Number.isFinite(v)?(v>0?'+':'')+v.toFixed(2):'—';
  function mapPoint(e){const rect=canvas.getBoundingClientRect();return {x:view.x+(e.clientX-rect.left)/rect.width*view.width,y:view.y+(e.clientY-rect.top)/rect.height*view.height,tol:Math.max(3,view.width/rect.width*8)};}
  function planetSigma(i){
    const b=snapshot?.diagnostic?.map_belief,c=b?.covariance,n=b?.planets?.length||0;
    if(!Array.isArray(c)||c.length!==3*n)return null;
    const s=k=>{const d=c[k]?.[k];return typeof d==='number'&&d>=0?Math.sqrt(d):null;};
    const x=s(3*i),y=s(3*i+1),mass=s(3*i+2);
    return x===null||y===null?null:{x,y,mass,position:Math.hypot(x,y)};
  }
  function nearestPlanetIndex(x,y,tol,planets){let best=-1,score=Infinity;planets.forEach((p,i)=>{const d=Math.hypot(x-p.x,y-p.y)-p.radius;if(d<=tol&&d<score){score=d;best=i;}});return best;}
  function segmentDistance(x,y,a,b){const dx=b.x-a.x,dy=b.y-a.y,t=Math.max(0,Math.min(1,((x-a.x)*dx+(y-a.y)*dy)/(dx*dx+dy*dy||1)));return Math.hypot(x-(a.x+t*dx),y-(a.y+t*dy));}
  // Only layers that are currently drawn can be hovered or selected.
  function hitTest(x,y,tol){
    const d=snapshot?.diagnostic,belief=d?.map_belief||{};
    for(const [id,p] of Object.entries(snapshot.world?.players||{}))if(Math.hypot(x-p.x,y-p.y)<=8+tol)return {kind:'player',id};
    if($('spawns').checked){const index=(d?.bot_state?.observed_spawn_locations||[]).findIndex(p=>Math.hypot(x-p.x,y-p.y)<=8+tol);if(index>=0)return {kind:'spawn',index};}
    if($('contacts').checked){const index=(belief.contact_points||[]).findIndex(p=>Math.hypot(x-p.x,y-p.y)<=3+tol);if(index>=0)return {kind:'contact',index};}
    if($('beliefPlanets').checked){const index=nearestPlanetIndex(x,y,tol,belief.planets||[]);if(index>=0)return {kind:'planet',index};}
    if($('actualPlanets').checked){const index=nearestPlanetIndex(x,y,tol,snapshot.actual_planets||[]);if(index>=0)return {kind:'actual',index};}
    if($('paths').checked){
      let best=null;
      for(const p of snapshot.paths||[]){const pts=p.points;let dist=Infinity;for(let i=2;i<pts.length;i+=2)dist=Math.min(dist,segmentDistance(x,y,{x:pts[i-2],y:pts[i-1]},{x:pts[i],y:pts[i+1]}));if(dist<=tol&&(!best||dist<best.dist||(dist===best.dist&&p.sequence>best.sequence)))best={dist,sequence:p.sequence};}
      if(best)return {kind:'shot',sequence:best.sequence};
    }
    return null;
  }
  // 0° = right, 90° = down (screen coordinates, y grows downward).
  const COMPASS=['→','↘','↓','↙','←','↖','↑','↗'];
  function dirText(v){const m=Math.hypot(v.x,v.y);if(!(m>0))return '0';const a=(Math.atan2(v.y,v.x)*180/Math.PI+360)%360;return `${fmt(m,6)} ${COMPASS[Math.round(a/45)%8]} ${fmt(a,0)}°`;}
  function pointRows(x,y){
    const p=gridAt(x,y),l=shownLayer(),b=snapshot.diagnostic?.map_belief||{};
    const near=(point,planets)=>{const n=nearestSurface(point,planets);return n?`#${n.index} · ${fmt(n.distance,1)} from center · surface residual ${fmt(n.residual,1)}`:'none';};
    const AXES='Value is |g| then direction: the arrow and angle are on screen, 0° right and 90° down.';
    const carried=snapshot.field_grid.carried?.[l],scalar=l==='uncertainty'?['Field σ (fit covariance)',Number.isFinite(p?.sigma)?fmt(p.sigma,6)+(carried?` · carried from #${carried}`:''):'unavailable','Spread of the gravity belief implied by the planet-fit covariance. Ignores unseen planets and model error.']:null;
    const belief=['Belief pull',p?.total?dirText(p.total):'no value here',`Gravity the bot simulates with at this point: the fitted planets. ${AXES}`];
    const truth=['Truth pull',p?.oracle?dirText(p.oracle):'no truth map',`Gravity of the truth map at this point (the planets the server disclosed; diagnosis only, never fit input). ${AXES}`];
    const error=['Error (belief − truth)',p?.delta?`${dirText(p.delta)}${p.oracle&&Math.hypot(p.oracle.x,p.oracle.y)>0?` · ${fmt(100*Math.hypot(p.delta.x,p.delta.y)/Math.hypot(p.oracle.x,p.oracle.y),0)}% of truth`:''}`:'unavailable','The arrow points to where the belief pulls extra compared with truth; the opposite way means the belief pulls too little.'];
    return [...(scalar?[scalar]:[]),...(l==='error'?[error,belief,truth]:[belief,truth,error]),
      ['Position',`(${fmt(x,1)}, ${fmt(y,1)})`],['Nearest estimated planet',near({x,y},b.planets)],['Nearest truth planet',snapshot.actual_planets?near({x,y},snapshot.actual_planets):'unavailable']];
  }
  // Returns {title, rows} for a selection, or null when the object is not in this snapshot.
  function describe(o){
    const d=snapshot?.diagnostic,b=d?.map_belief||{},actual=snapshot?.actual_planets;
    if(o.kind==='planet'){
      const p=b.planets?.[o.index];if(!p)return null;
      const s=planetSigma(o.index),match=planetMatches().find(m=>m.estimatedIndex===o.index);
      return {title:`Estimated planet #${o.index}`,rows:[['Center',`(${fmt(p.x,1)}, ${fmt(p.y,1)})`],['Radius',fmt(p.radius,1)],['Mass',fmt(p.mass,0)],['Position σ (x, y)',s?`${fmt(s.x,1)}, ${fmt(s.y,1)}`:'unavailable'],['Mass σ',s?fmt(s.mass,0):'unavailable'],['Truth match',!actual?'no truth map':match?`#${match.index} · ${STATUS_LABEL[matchStatus(match)]} · ${matchNumbers(match)}`:'no truth planet within 80 units (unmatched estimate)']]};
    }
    if(o.kind==='actual'){
      const p=actual?.[o.index];if(!p)return null;
      const match=planetMatches().find(m=>m.index===o.index);
      const st=disclosedStatuses()[o.index];
      return {title:`Truth planet #${o.index}`,rows:[['Center',`(${fmt(p.x,1)}, ${fmt(p.y,1)})`],['Radius',fmt(p.radius,1)],['Mass',fmt(p.mass,0)],['Status',`${STATUS_LABEL[st.status]} (center > ${LOCATION_TOL} or radius > ${SIZE_TOL} units off)`],['Estimated match',match?`#${match.estimatedIndex} · ${matchNumbers(match)}`:'no estimate within 80 units']]};
    }
    if(o.kind==='player'){
      const p=snapshot.world?.players?.[o.id];if(!p)return null;
      const flying=(snapshot.world?.in_flight||[]).filter(s=>String(s.player_id)===String(o.id)).map(s=>`v${fmt(s.velocity,0)} @ ${fmt(s.angle_degrees,0)}°`);
      return {title:`Player ${pname(o.id)}${String(snapshot.world?.own_id)===String(o.id)?' (this bot)':''}`,rows:[['Last known position',`(${fmt(p.x,1)}, ${fmt(p.y,1)})`],['Shots in flight',flying.join(', ')||'none']]};
    }
    if(o.kind==='spawn'){
      const p=d?.bot_state?.observed_spawn_locations?.[o.index];if(!p)return null;
      return {title:`Observed spawn sample #${o.index}`,rows:[['Position',`(${fmt(p.x,1)}, ${fmt(p.y,1)})`],['Meaning','a place a player was seen to appear in this map epoch; the set is incomplete and new spawns elsewhere are possible']]};
    }
    if(o.kind==='contact'){
      const c=b.contact_points?.[o.index];if(!c)return null;
      const e=nearestSurface(c,b.planets),a=nearestSurface(c,actual);
      return {title:`Accepted contact #${o.index}`,rows:[['Position',`(${fmt(c.x,1)}, ${fmt(c.y,1)})`],['Nearest estimated planet',e?`#${e.index} · surface residual ${fmt(e.residual,2)}`:'none'],['Nearest truth planet',a?`#${a.index} · surface residual ${fmt(a.residual,2)}`:'unavailable']]};
    }
    if(o.kind==='shot'){
      const e=(snapshot.paths||[]).find(p=>p.sequence===o.sequence);if(!e?.points.length)return null;const end=pathEnd(e);
      const est=nearestSurface(end,b.planets),act=nearestSurface(end,actual);
      return {title:`Trajectory${e.label?` (${e.label})`:''} · event #${o.sequence}`,rows:[['Outcome (inferred)',OUTCOME_LABELS[e.outcome]||e.outcome],['Player',pname(e.player_id)],...(e.label?[['Scenario role',e.label]]:[]),['Shot',`v${fmt(e.velocity,0)} at ${fmt(e.angle,1)}°`],['Samples',String(e.samples)],['Endpoint',`(${fmt(end.x,1)}, ${fmt(end.y,1)})`],['Nearest estimated planet',est?`#${est.index} · surface residual ${fmt(est.residual,2)}`:'none'],['Nearest truth planet',act?`#${act.index} · surface residual ${fmt(act.residual,2)}${act.residual<1?' · on surface (likely impact)':''}`:'unavailable'],['Belief snapshot',`#${snapshot.diagnostic_sequence??'unavailable'}${d?.map_updating?' · map update pending':''}`]]};
    }
    if(o.kind==='point')return {title:'Map point',rows:pointRows(o.x,o.y)};
    return null;
  }
  function renderSelection(){
    const el=$('selection'),info=sel&&snapshot?describe(sel):null;
    if(!info){kv(el,[[sel?'The selected object is not in this snapshot.':'Hover the map for a summary; click a planet, player, contact or trajectory to pin it.',null]]);return;}
    kv(el,[[info.title,null],...info.rows]);
  }
  function drawSelection(px){
    if(!sel||!snapshot)return;
    const b=snapshot.diagnostic?.map_belief||{};
    const ring=(x,y,r)=>{ctx.beginPath();ctx.arc(x,y,r,0,Math.PI*2);ctx.strokeStyle='#ffe27a';ctx.lineWidth=3*px;ctx.stroke();};
    if(sel.kind==='planet'){const p=b.planets?.[sel.index];if(p)ring(p.x,p.y,p.radius+5*px);}
    else if(sel.kind==='actual'){const p=snapshot.actual_planets?.[sel.index];if(p)ring(p.x,p.y,p.radius+5*px);}
    else if(sel.kind==='player'){const p=snapshot.world?.players?.[sel.id];if(p)ring(p.x,p.y,13*px);}
    else if(sel.kind==='contact'){const p=b.contact_points?.[sel.index];if(p)ring(p.x,p.y,8*px);}
    else if(sel.kind==='spawn'){const p=snapshot.diagnostic?.bot_state?.observed_spawn_locations?.[sel.index];if(p)ring(p.x,p.y,13*px);}
    else if(sel.kind==='point')ring(sel.x,sel.y,5*px);
  }
  const matchNumbers=m=>`center ${fmt(m.distance,1)} off · radius ${sgn(m.radiusError)} · mass ×${fmt(m.massRatio,2)}`;
  function mapAgeRows(){const d=snapshot?.diagnostic||{},rows=[],lag=lagText(currentDecision());if(lag)rows.push(['Belief used',lag]);if(d.map_wait_ms!=null)rows.push(['Waited for map',`${d.map_wait_ms} ms${d.map_wait_fallback?' (gave up waiting)':''}`]);return rows;}
  canvas.addEventListener('mousemove',e=>{
    if(!snapshot||viewDrag)return;
    const m=mapPoint(e),hit=hitTest(m.x,m.y,m.tol),info=hit?describe(hit):null;
    const shown=info||describe({kind:'point',x:m.x,y:m.y}),lines=[shown.title,...shown.rows.slice(0,3).map(([a,v])=>`${a}: ${v}`)];
    tip.replaceChildren(...lines.map((t,i)=>{const d=document.createElement('div');if(i===0)d.style.fontWeight='600';d.textContent=t;return d;}));
    tip.style.display='block';tip.style.left=`${Math.min(e.clientX+14,window.innerWidth-340)}px`;tip.style.top=`${Math.min(e.clientY+14,window.innerHeight-90)}px`;
    canvas.style.cursor=hit?'pointer':'crosshair';
  });
  canvas.addEventListener('mouseleave',()=>{tip.style.display='none';});
  canvas.addEventListener('click',e=>{if(panned){panned=false;return;}if(!snapshot||viewDrag)return;const m=mapPoint(e);sel=hitTest(m.x,m.y,m.tol)||{kind:'point',x:m.x,y:m.y};render();});
  // The sidebar is resizable; its width is remembered in this browser.
  const handle=$('sideHandle'),main=document.querySelector('main'),setSide=px=>{main.style.setProperty('--side',`${Math.round(Math.max(300,Math.min(900,px)))}px`);};
  if(Number(localStorage.getItem('sideWidth')))setSide(Number(localStorage.getItem('sideWidth')));
  handle.addEventListener('pointerdown',e=>{handle.setPointerCapture(e.pointerId);handle.dataset.drag='1';});
  handle.addEventListener('pointermove',e=>{if(handle.dataset.drag!=='1')return;setSide(window.innerWidth-e.clientX);render();drawScrub();});
  handle.addEventListener('pointerup',()=>{handle.dataset.drag='';localStorage.setItem('sideWidth',getComputedStyle(main).getPropertyValue('--side').replace('px',''));});
  loadOverview();
  fillCaptureSelect().catch(e=>setStatus(e.message));
})();
