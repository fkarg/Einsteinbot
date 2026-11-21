(() => {
  const W=1885.618,H=1060.66;
  const $=id=>document.getElementById(id);
  const canvas=$('map'),ctx=canvas.getContext('2d');
  let captureId=null, decisions=[], snapshot=null, simulations=[],timeline=[],sel=null,selScope=null,fieldAutoFallback=null,snapshotRequest=0;
  let view={x:0,y:0,width:W,height:H}, viewDrag=null, panned=false;
  const colors=['#54c8ff','#ffb45c','#b791ff','#7de3a1','#ff7899','#e8d96d','#88a7ff','#ffa1dc'];
  function setStatus(s){ $('status').textContent=s||''; }
  function fmt(v,n=2){ return v===null||v===undefined||v===''?'—':Number.isFinite(Number(v))?Number(v).toFixed(n):'—'; }
  function selected(){ return Number($('decision').value); }
  function q(v){return encodeURIComponent(v);}
  async function api(path){const r=await fetch(path,{cache:'no-store'});const data=await r.json();if(!r.ok||data.error)throw new Error(data.error||r.statusText);return data;}
  async function loadCaptures(){
    setStatus('Loading captures…');
    const data=await api('/api/captures'); const select=$('capture'); select.replaceChildren();
    for(const c of data.captures){const option=document.createElement('option');option.value=c.id;const date=new Date(c.started_at_unix_ms).toLocaleString();const kind=c.metadata?.host==='synthetic-scenario'?'scenario':'live',revision=c.metadata?.build?.git_revision;option.textContent=`#${c.id} · ${kind} · ${c.metadata?.client_name||'capture'}${revision?` · ${revision.slice(0,8)}`:''} · ${date}`;select.append(option);}
    if(!data.captures.length){setStatus('No captures in this database');return;}
    captureId=Number(select.value); await loadDecisions();
  }
  async function loadDecisions(preferredBot=null){
    captureId=Number($('capture').value); const data=await api(`/api/decisions?id=${captureId}`); decisions=data.decisions;
    const bots=[...new Set(decisions.map(d=>d.bot_name))]; const bot=$('bot');const old=preferredBot||bot.value;bot.replaceChildren();
    for(const name of ['all',...bots]){const op=document.createElement('option');op.value=name;op.textContent=name==='all'?'All bots':name;bot.append(op);}
    bot.value=bots.includes(old)?old:'all'; fillDecisionList();
  }
  function fillDecisionList(){
    const chosenBot=$('bot').value;const filtered=decisions.filter(d=>chosenBot==='all'||d.bot_name===chosenBot);const sel=$('decision');sel.replaceChildren();
    const none=document.createElement('option');none.value='none';none.textContent='No decision · latest capture state';sel.append(none);
    for(const d of filtered){const op=document.createElement('option');op.value=d.sequence;const c=d.choice||{};const label=d.scenario?.stage||`${c.reason||'no shot'} v${c.velocity??'–'} @ ${c.angle_degrees??'–'}°`;op.textContent=`#${d.sequence} ${d.bot_name} · ${label}`;sel.append(op);}
    sel.value='none'; loadSnapshot(); loadTimeline();
  }
  async function loadSnapshot(){
    if(!$('decision').value)return;
    const request=++snapshotRequest,requestCapture=captureId,requestDecision=$('decision').value,requestField=$('field').value,requestView={...view};
    setStatus('Loading captured state…');simulations=[];
    try{const step=gridStepForView(),params=`id=${requestCapture}&seq=${requestDecision}&step=${step}&field=${requestField}&x=${requestView.x}&y=${requestView.y}&width=${requestView.width}&height=${requestView.height}`;const result=await api(`/api/snapshot?${params}`);if(request!==snapshotRequest||requestCapture!==captureId||requestDecision!==$('decision').value||requestField!==$('field').value||requestView.x!==view.x||requestView.y!==view.y||requestView.width!==view.width||requestView.height!==view.height)return;snapshot=result;const scope=`${captureId}|${requestDecision}`;if(scope!==selScope){selScope=scope;sel=snapshot.selected_shot?{kind:'shot',...snapshot.selected_shot}:null;}const needsActual=requestField==='delta'||requestField==='hypothesis_delta';if(!snapshot.actual_planets&&needsActual){fieldAutoFallback=requestField;$('field').value=requestField==='delta'?'total':'hypothesis_total';return loadSnapshot();}else if(snapshot.actual_planets&&fieldAutoFallback){$('field').value=fieldAutoFallback;fieldAutoFallback=null;return loadSnapshot();}const g=snapshot.field_grid;g.samples=g.samples.map(s=>{const pair=(x,y)=>Number.isFinite(x)&&Number.isFinite(y)?{x,y}:null,vector=pair(s[2],s[3]),total=pair(s[4],s[5]),physical=pair(s[6],s[7]),residual=pair(s[8],s[9]),oracle=pair(s[10],s[11]),delta=total&&oracle?{x:total.x-oracle.x,y:total.y-oracle.y}:null;return {x:s[0],y:s[1],vector,total,physical,residual,oracle,delta,sigma:s[12],supported:s[13],gravity:total?Math.hypot(total.x,total.y):null,actual_gravity:oracle?Math.hypot(oracle.x,oracle.y):null};});const c=currentDecision().choice||{};$('velocity').value=c.velocity||8;$('angle').value=c.angle_degrees||0;setStatus(step===Number($('resolution').value)?'':`Showing ${step}-unit samples to fit this viewport`);render();}
    catch(e){setStatus(e.message);}
  }
  async function loadTimeline(){try{const data=await api(`/api/timeline?id=${captureId}`);timeline=data.timeline;const el=$('timeline');el.replaceChildren();for(const item of timeline){const b=document.createElement('button');b.textContent=`#${item.sequence} ${item.kind} · ${item.detail}`;b.title=new Date(item.timestamp_unix_ms).toLocaleString();b.className=String(snapshot?.sequence)===String(item.sequence)?'active':'';b.addEventListener('click',()=>{const select=$('decision'),value=item.kind==='decision'&&decisions.some(d=>d.sequence===item.sequence)?String(item.sequence):`event:${item.sequence}`;if(![...select.options].some(option=>option.value===value)){const option=document.createElement('option');option.value=value;option.textContent=`#${item.sequence} · ${item.kind} · ${item.detail}`;select.append(option);}select.value=value;loadSnapshot();});el.append(b);}}catch(e){setStatus(e.message);}}
  function currentDecision(){return decisions.find(d=>d.sequence===selected())||{};}
  function sampleCountForStep(step){const startX=Math.floor(view.x/step)*step,startY=Math.floor(view.y/step)*step,endX=Math.ceil(view.x+view.width),endY=Math.ceil(view.y+view.height),columns=Math.floor((endX-startX)/step)+1,rows=Math.floor((endY-startY)/step)+1;return columns*rows;}
  function gridStepForView(){const desired=Number($('resolution').value);return [1,2,3,5,10,25].find(step=>step>=desired&&sampleCountForStep(step)<=300000)||25;}
  function gridAt(x,y){const g=snapshot?.field_grid;if(!g)return null;const col=Math.max(0,Math.min(g.columns-1,Math.round((x-g.origin_x)/g.step))),row=Math.max(0,Math.min(g.rows-1,Math.round((y-g.origin_y)/g.step)));return g.samples[row*g.columns+col]||null;}
  function selectedField(){return $('field').value;}
  function fieldVector(p){return p?.vector||null;}
  function heatValue(p){const mode=$('heat').value;if(mode==='support')return p.supported?1:0;if(mode==='sigma')return p.sigma;const v=fieldVector(p);if(!v)return null;if(mode==='x')return v.x;if(mode==='y')return v.y;return Math.hypot(v.x,v.y);}
  function heatColor(t){const hue=240-240*Math.max(0,Math.min(1,t));return `hsla(${hue},85%,56%,.52)`;}
  function render(){
    const dpr=window.devicePixelRatio||1,rect=canvas.getBoundingClientRect();if(rect.width){canvas.width=Math.round(rect.width*dpr);canvas.height=Math.round(rect.width*H/W*dpr);}const sx=canvas.width/view.width,sy=canvas.height/view.height;ctx.setTransform(sx,0,0,sy,-view.x*sx,-view.y*sy);ctx.fillStyle='#080d13';ctx.fillRect(view.x,view.y,view.width,view.height);
    if(!snapshot){return;}
    const hasDecision=snapshot.selection_kind==='decision';for(const id of ['simulate','compare','audit'])$(id).disabled=!hasDecision;
    const samples=snapshot.field_grid?.samples||[];const values=samples.map(heatValue).filter(Number.isFinite).sort((a,b)=>a-b),scale=$('scale').value;
    const transform=v=>scale==='log'?Math.sign(v)*Math.log1p(Math.abs(v)):v;
    const quantile=p=>values.length?values[Math.min(values.length-1,Math.floor((values.length-1)*p))]:null;
    const rawLo=scale==='percentile'?quantile(.05):quantile(0),rawHi=scale==='percentile'?quantile(.95):quantile(1);let lo=Number.isFinite(rawLo)?rawLo:0,hi=Number.isFinite(rawHi)?rawHi:1;if(scale==='log'){lo=transform(lo);hi=transform(hi);}if(hi<=lo)hi=lo+1;
    $('scaleMin').textContent=Number.isFinite(rawLo)?fmt(rawLo,4):'no data';$('scaleMax').textContent=Number.isFinite(rawHi)?fmt(rawHi,4):'no data';$('scaleBar').style.background=$('heat').value==='support'?'linear-gradient(90deg,#080d13,#2aaa8c)':'linear-gradient(90deg,hsl(240 85% 56%),hsl(180 85% 56%),hsl(120 85% 56%),hsl(60 85% 56%),hsl(0 85% 56%))';$('scaleBar').title=`${scale==='percentile'?'5th–95th percentiles':'Full range'} · ${fmt(rawLo,6)} to ${fmt(rawHi,6)} · ${$('heat').value==='sigma'?'uncertainty magnitude':$('heat').value==='support'?'support (0–1)':$('heat').value==='x'?'signed X component':$('heat').value==='y'?'signed Y component':'field strength'}`;
    for(const p of samples){const raw=heatValue(p);if(!Number.isFinite(raw))continue;const v=transform(raw),t=Math.max(0,Math.min(1,(v-lo)/(hi-lo)));ctx.fillStyle=$('heat').value==='support'?(p.supported?'rgba(42,170,140,.35)':'rgba(0,0,0,.22)'):heatColor(t);ctx.fillRect(p.x,p.y, snapshot.field_grid.step,snapshot.field_grid.step);}
    if($('arrows').checked){const spacing=Math.max(24,75*view.width/W),skip=Math.max(1,Math.round(spacing/snapshot.field_grid.step)),planets=snapshot.diagnostic?.map_belief?.planets||[];for(let i=0;i<samples.length;i++){const p=samples[i],col=i%snapshot.field_grid.columns,row=Math.floor(i/snapshot.field_grid.columns);if(col%skip||row%skip||planets.some(o=>(p.x-o.x)**2+(p.y-o.y)**2<=o.radius**2))continue;const v=fieldVector(p);if(!v)continue;const mag=Math.hypot(v.x,v.y);if(!mag)continue;const len=Math.min(18,5+Math.log1p(mag)*4),ux=v.x/mag,uy=v.y/mag;ctx.strokeStyle='#eef6ffcc';ctx.fillStyle='#eef6ffcc';ctx.lineWidth=1.4;ctx.beginPath();ctx.moveTo(p.x-ux*len*.35,p.y-uy*len*.35);ctx.lineTo(p.x+ux*len*.65,p.y+uy*len*.65);ctx.stroke();ctx.beginPath();ctx.moveTo(p.x+ux*len*.65,p.y+uy*len*.65);ctx.lineTo(p.x+ux*len*.65-ux*5-uy*3,p.y+uy*len*.65-uy*5+ux*3);ctx.lineTo(p.x+ux*len*.65-ux*5+uy*3,p.y+uy*len*.65-uy*5-ux*3);ctx.fill();}}
    if($('pathResiduals').checked){const residuals=(snapshot.events||[]).flatMap(e=>e.residual_samples||[]),mags=residuals.map(r=>Math.hypot(r[2],r[3])).sort((a,b)=>a-b),cap=mags.length?mags[Math.floor((mags.length-1)*.95)]:null;$('pathScale').style.display=residuals.length?'grid':'none';$('pathMax').textContent=Number.isFinite(cap)?fmt(cap,4):'no data';$('pathScaleBar').title=`Path residual magnitude, colors clip at the 95th percentile (${fmt(cap,6)})`;for(const r of residuals){const t=Math.max(0,Math.min(1,Math.hypot(r[2],r[3])/(cap||1)));ctx.beginPath();ctx.arc(r[0],r[1],1.7,0,Math.PI*2);ctx.fillStyle=heatColor(t);ctx.fill();}}else $('pathScale').style.display='none';
    if($('coverage').checked){const cells=snapshot.diagnostic?.bot_state?.covered_cells||[];ctx.fillStyle='rgba(102,148,212,.2)';for(const c of cells)ctx.fillRect((Number(c[0])+.5)*20,(Number(c[1])+.5)*20,20,20);}
    if($('paths').checked){const paths=(snapshot.events||[]).filter(e=>e.record?.event?.type==='shot_fin');for(const item of paths){const e=item.record.event;if(!e.points?.length)continue;ctx.beginPath();ctx.moveTo(e.points[0].x,e.points[0].y);for(const p of e.points)ctx.lineTo(p.x,p.y);ctx.strokeStyle=colors[e.player_id%colors.length]+'55';ctx.lineWidth=1.2;ctx.stroke();const end=e.points.at(-1);ctx.beginPath();ctx.arc(end.x,end.y,2.8,0,Math.PI*2);ctx.fillStyle=colors[e.player_id%colors.length]+'bb';ctx.fill();}}
    if($('beliefPlanets').checked){for(const p of snapshot.diagnostic?.map_belief?.planets||[])circle(p,'#ffc45b',true);}
    if($('actualPlanets').checked){for(const p of snapshot.actual_planets||[])circle(p,'#60dcff',false);}
    if($('contacts').checked){ctx.fillStyle='#c58cff';for(const p of snapshot.diagnostic?.map_belief?.contact_points||[]){ctx.beginPath();ctx.arc(p.x,p.y,3,0,Math.PI*2);ctx.fill();}}
    const players=snapshot.diagnostic?.bot_state?.player_positions||{};for(const [id,p] of Object.entries(players)){ctx.beginPath();ctx.arc(p.x,p.y,id===String(snapshot.diagnostic?.bot_state?.own_id)?8:6,0,Math.PI*2);ctx.fillStyle=id===String(snapshot.diagnostic?.bot_state?.own_id)?'#f4f7ff':colors[Number(id)%colors.length];ctx.fill();ctx.strokeStyle='#080d13';ctx.lineWidth=2;ctx.stroke();ctx.fillStyle='#fff';ctx.font='12px system-ui';ctx.fillText(id,p.x+8,p.y-8);}
    for(const sim of simulations){const pts=sim.data.points||[];if(!pts.length)continue;ctx.beginPath();ctx.moveTo(pts[0].x,pts[0].y);for(const p of pts)ctx.lineTo(p.x,p.y);ctx.strokeStyle=sim.color;ctx.lineWidth=3;ctx.stroke();}
    if(sel?.kind==='shot'&&sel.event?.points?.length){const pts=sel.event.points;ctx.beginPath();ctx.moveTo(pts[0].x,pts[0].y);for(const p of pts)ctx.lineTo(p.x,p.y);ctx.strokeStyle='#fff2a8';ctx.lineWidth=3;ctx.stroke();const end=pts.at(-1);ctx.beginPath();ctx.arc(end.x,end.y,6,0,Math.PI*2);ctx.strokeStyle='#fff2a8';ctx.lineWidth=2;ctx.stroke();}
    drawSelection();
    const decision=currentDecision(),choice=decision.choice||{};$('decisionLabel').textContent=snapshot.selection_kind==='decision'?`Capture ${captureId}, decision ${decision.sequence} · ${decision.bot_name} · ${new Date(decision.timestamp_unix_ms||0).toLocaleTimeString()} · map updating: ${decision.map_updating?'yes':'no'}`:`Capture ${captureId}, no decision selected · showing capture events through #${snapshot.sequence}; last belief snapshot #${snapshot.diagnostic_sequence??'unavailable'}`;
    renderMetrics(decision);renderChoice(choice);renderPlanetList();renderSelection();
  }
  function circle(p,color,fill){ctx.beginPath();ctx.arc(p.x,p.y,Math.max(3,p.radius||3),0,Math.PI*2);ctx.lineWidth=2;ctx.strokeStyle=color;if(fill){ctx.fillStyle=color+'26';ctx.fill();}else ctx.setLineDash([7,5]);ctx.stroke();ctx.setLineDash([]);}
  function kv(el,rows){el.replaceChildren();for(const [a,b] of rows){if(b===null){const h=document.createElement('b');h.className='wide';h.textContent=a;el.append(h);continue;}const x=document.createElement('span');x.textContent=a;const y=document.createElement('b');y.textContent=b;el.append(x,y);}}
  function renderMetrics(d){
    const b=snapshot.diagnostic?.map_belief||{},g=snapshot.field_grid||{},cov=b.covariance;
    const sigmas=g.samples?.map(p=>p.sigma).filter(Number.isFinite)||[];const sorted=[...sigmas].sort((a,b)=>a-b);const median=sorted.length?sorted[Math.floor(sorted.length/2)]:null;
    const local=b.local_field?.samples?.length||0;
    const delta=snapshot.field_grid?.samples?.map(p=>p.delta).filter(v=>v&&Number.isFinite(v.x)&&Number.isFinite(v.y))||[],errors=delta.map(v=>Math.hypot(v.x,v.y)).sort((a,b)=>a-b),fieldRmse=delta.length?Math.sqrt(delta.reduce((sum,v)=>sum+v.x*v.x+v.y*v.y,0)/delta.length):null;
    const matches=planetMatches(b.planets||[],snapshot.actual_planets||[]);const centerErrors=matches.map(m=>m.distance).sort((a,b)=>a-b),radiusErrors=matches.map(m=>Math.abs(m.estimated.radius-m.actual.radius)).sort((a,b)=>a-b);
    const stale=d.map_updating===true;
    kv($('metrics'),[['Map state',stale?'stale · fitting newer shots':'latest available'],['Planets · estimate / actual',`${b.planets?.length??0} / ${snapshot.actual_planets?.length??'—'}`],['Matched centers ≤80 units',String(matches.length)],['Median center / radius error',matches.length?`${fmt(centerErrors[Math.floor(centerErrors.length/2)])} / ${fmt(radiusErrors[Math.floor(radiusErrors.length/2)])} units`:'—'],['Gravity error · RMS / p95 (viewport)',errors.length?`${fmt(fieldRmse)} / ${fmt(errors[Math.floor(errors.length*.95)])}`:'actual map unavailable'],['Fit trajectory RMSE',`${fmt(b.model_position_rmse)} units`],['Accepted impact points',String(b.contact_points?.length??0)],['Local residual samples',String(local)],['Fit converged',b.fit_converged===null||b.fit_converged===undefined?'—':b.fit_converged?'yes':'no'],['Ambiguous associations',String(b.uncertain_associations??0)],['Fit covariance',g.has_covariance?'available':'unavailable']]);
  }
  function renderChoice(c){kv($('choice'),[['Reason',String(c.reason||'—')],['Selected velocity',String(c.velocity??'—')],['Selected angle',`${c.angle_degrees??'—'}°`],['Predicted victim',String(c.predicted_victim??'—')],['Predicted center miss',`${fmt(c.miss_distance)} units`],['Alternative model agrees',String(c.alternative_agrees??'untested')],['Field hypothesis used',String(c.used_field_hypothesis??false)],['Covered path fraction',fmt(c.covered_fraction,3)]]);}
  function planetMatches(estimated,actual){const used=new Set(),matches=[];for(const estimate of estimated){let best=null;actual.forEach((planet,index)=>{if(used.has(index))return;const distance=Math.hypot(estimate.x-planet.x,estimate.y-planet.y);if(distance<=80&&(!best||distance<best.distance))best={index,actual:planet,estimated:estimate,distance};});if(best){used.add(best.index);matches.push(best);}}return matches;}
  function nearestSurface(point,planets){return planets?.map((p,i)=>({planet:p,index:i,distance:Math.hypot(point.x-p.x,point.y-p.y),residual:Math.abs(Math.hypot(point.x-p.x,point.y-p.y)-p.radius)})).sort((a,b)=>a.residual-b.residual)[0]||null;}
  async function simulate(){const d=currentDecision();if(!d.sequence)return;try{setStatus('Simulating…');const query=`id=${captureId}&seq=${d.sequence}&velocity=${q($('velocity').value)}&angle=${q($('angle').value)}&model=${$('model').value}`;const result=await api(`/api/simulate?${query}`);simulations=[{data:result,color:result.model==='actual'?'#ff8d68':'#8ee5a4'}];setStatus(`${result.termination.kind.replaceAll('_',' ')}${result.termination.player_id!==undefined?` · player ${result.termination.player_id}`:''}`);render();}catch(e){setStatus(e.message);}}
  async function compare(){const d=currentDecision();try{setStatus('Comparing with disclosed map…');const result=await api(`/api/compare?id=${captureId}&seq=${d.sequence}`);simulations=[];
      const recorded=result.recorded_choice, runs=[];const sim=(choice,model,color,label)=>api(`/api/simulate?id=${captureId}&seq=${d.sequence}&velocity=${choice.velocity}&angle=${choice.angle_degrees}&model=${model}`).then(data=>{simulations.push({data,color,label});runs.push({label,termination:data.termination});});
      if(recorded?.velocity){const model=recorded.used_field_hypothesis?'hypothesis':'belief';await sim(recorded,model,'#8ee5a4','Recorded shot, captured belief');await sim(recorded,'actual','#ff8d68','Recorded shot, disclosed map');}
      const oracle=result.actual_map_choice_current_policy;if(oracle?.velocity)await sim(oracle,'actual','#ffe27a','Current policy, disclosed map');
      const choiceText=c=>c?.velocity?`v${c.velocity} @ ${c.angle_degrees}° · ${c.reason||'shot'}${c.predicted_victim!=null?` · victim ${c.predicted_victim}`:''}`:'no shot',endText=r=>`${r.termination.kind.replaceAll('_',' ')}${r.termination.player_id!==undefined?` · player ${r.termination.player_id}`:''}`;kv($('comparison'),[['Recorded choice',choiceText(recorded)],['Actual-map choice (current policy)',choiceText(oracle)],...runs.map(r=>[r.label,endText(r)]),...(result.note?[[result.note,null]]:[])]);
      setStatus('Green: belief · orange: recorded shot on actual map · yellow: current policy on actual map');render();
    }catch(e){$('comparison').textContent=e.message;setStatus(e.message);}}
  async function audit(){const d=currentDecision();try{setStatus('Replaying observed paths…');const result=await api(`/api/physics-audit?id=${captureId}&seq=${d.sequence}`);simulations=[{data:{points:result.worst_observed_points},color:'#d7e2ff'},{data:{points:result.worst_rust_points},color:'#ff4da6'}];kv($('comparison'),[['Audited shots',String(result.audited_shots)],['Planets in map',String(result.map_planets)],['Timed samples',String(result.timed_samples)],['Coordinate RMSE',fmt(result.coordinate_rmse)],['Median / p95 / max position error',`${fmt(result.median_position_error)} / ${fmt(result.p95_position_error)} / ${fmt(result.max_position_error)}`],['Worst shot',(w=>w?`event #${w.shot_sequence} · player ${w.player_id} · v${fmt(w.velocity,0)} @ ${fmt(w.angle,1)}° · RMSE ${fmt(w.coordinate_rmse)} · max ${fmt(w.max_position_error)} · Rust: ${String(w.rust_termination?.kind||'—').replaceAll('_',' ')}`:'—')(result.worst_shot)],...(result.note?[[result.note,null]]:[])]);setStatus(`Rust path replay: coordinate RMSE ${fmt(result.coordinate_rmse)} units · worst shot overlaid`);render();}catch(e){$('comparison').textContent=e.message;setStatus(e.message);}}
  $('capture').addEventListener('change',()=>{resetView();loadDecisions();});$('bot').addEventListener('change',fillDecisionList);$('decision').addEventListener('change',loadSnapshot);$('latest').addEventListener('click',()=>{$('decision').value='none';loadSnapshot();});$('refresh').addEventListener('click',()=>loadDecisions($('bot').value));$('heat').addEventListener('change',render);$('scale').addEventListener('change',render);$('field').addEventListener('change',()=>{fieldAutoFallback=null;loadSnapshot();});$('arrows').addEventListener('change',render);$('resolution').addEventListener('change',loadSnapshot);for(const id of ['paths','pathResiduals','coverage','beliefPlanets','actualPlanets','contacts'])$(id).addEventListener('change',render);$('simulate').addEventListener('click',simulate);$('compare').addEventListener('click',compare);$('audit').addEventListener('click',audit);window.addEventListener('resize',render);
  let viewportTimer=null;function refreshViewport(){clearTimeout(viewportTimer);viewportTimer=setTimeout(loadSnapshot,120);}
  function zoomAt(factor,px=W/2,py=H/2){const width=Math.max(40,Math.min(W,view.width*factor)),height=width*H/W,cx=view.x+px/W*view.width,cy=view.y+py/H*view.height;view={x:Math.max(0,Math.min(W-width,cx-px/W*width)),y:Math.max(0,Math.min(H-height,cy-py/H*height)),width,height};refreshViewport();}
  function resetView(){view={x:0,y:0,width:W,height:H};loadSnapshot();}
  $('zoomIn').addEventListener('click',()=>zoomAt(.6));$('zoomOut').addEventListener('click',()=>zoomAt(1.67));$('resetView').addEventListener('click',resetView);
  canvas.addEventListener('dblclick',resetView);
  canvas.addEventListener('pointerdown',e=>{panned=false;if(!e.shiftKey&&e.button!==1)return;const rect=canvas.getBoundingClientRect();viewDrag={clientX:e.clientX,clientY:e.clientY,rect,view:{...view}};canvas.setPointerCapture(e.pointerId);});
  canvas.addEventListener('pointermove',e=>{if(!viewDrag)return;const dx=(e.clientX-viewDrag.clientX)/viewDrag.rect.width*viewDrag.view.width,dy=(e.clientY-viewDrag.clientY)/viewDrag.rect.height*viewDrag.view.height;view={...viewDrag.view,x:Math.max(0,Math.min(W-viewDrag.view.width,viewDrag.view.x-dx)),y:Math.max(0,Math.min(H-viewDrag.view.height,viewDrag.view.y-dy))};render();});canvas.addEventListener('pointerup',()=>{if(viewDrag){viewDrag=null;panned=true;refreshViewport();}});
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
    const d=snapshot?.diagnostic,belief=d?.map_belief||{},bot=d?.bot_state||{};
    for(const [id,p] of Object.entries(bot.player_positions||{}))if(Math.hypot(x-p.x,y-p.y)<=8+tol)return {kind:'player',id};
    if($('contacts').checked){const index=(belief.contact_points||[]).findIndex(p=>Math.hypot(x-p.x,y-p.y)<=3+tol);if(index>=0)return {kind:'contact',index};}
    if($('beliefPlanets').checked){const index=nearestPlanetIndex(x,y,tol,belief.planets||[]);if(index>=0)return {kind:'planet',index};}
    if($('actualPlanets').checked){const index=nearestPlanetIndex(x,y,tol,snapshot.actual_planets||[]);if(index>=0)return {kind:'actual',index};}
    if($('paths').checked){
      let best=null;
      for(const item of snapshot.events||[]){const e=item.record?.event;if(e?.type!=='shot_fin')continue;let dist=Infinity;for(let i=1;i<(e.points?.length||0);i++)dist=Math.min(dist,segmentDistance(x,y,e.points[i-1],e.points[i]));if(dist<=tol&&(!best||dist<best.dist))best={dist,sequence:item.sequence,event:e};}
      if(best)return {kind:'shot',sequence:best.sequence,event:best.event};
    }
    return null;
  }
  function pointRows(x,y){
    const p=gridAt(x,y),v=fieldVector(p),b=snapshot.diagnostic?.map_belief||{};
    const near=(point,planets)=>{const n=nearestSurface(point,planets);return n?`#${n.index} · ${fmt(n.distance,1)} from center · surface residual ${fmt(n.residual,1)}`:'none';};
    return [[`${$('field').selectedOptions[0].text} |g|`,v?`${fmt(Math.hypot(v.x,v.y),6)} (${fmt(v.x,6)}, ${fmt(v.y,6)})`:'no value here'],['Parameter σ (field)',fmt(p?.sigma,6)],['Position',`(${fmt(x,1)}, ${fmt(y,1)})`],['Disclosed field',p?.oracle?`(${fmt(p.oracle.x,6)}, ${fmt(p.oracle.y,6)})`:'unavailable'],['Belief − disclosed',p?.delta?`(${fmt(p.delta.x,6)}, ${fmt(p.delta.y,6)})`:'unavailable'],['Nearest estimated planet',near({x,y},b.planets)],['Nearest disclosed planet',snapshot.actual_planets?near({x,y},snapshot.actual_planets):'unavailable']];
  }
  // Returns {title, rows} for a selection, or null when the object is not in this snapshot.
  function describe(o){
    const d=snapshot?.diagnostic,b=d?.map_belief||{},bot=d?.bot_state||{},actual=snapshot?.actual_planets;
    if(o.kind==='planet'){
      const p=b.planets?.[o.index];if(!p)return null;
      const s=planetSigma(o.index),match=planetMatches(b.planets,actual||[]).find(m=>m.estimated===p);
      return {title:`Estimated planet #${o.index}`,rows:[['Center',`(${fmt(p.x,1)}, ${fmt(p.y,1)})`],['Radius',fmt(p.radius,1)],['Mass',fmt(p.mass,0)],['Surface contacts',String(b.planet_contact_counts?.[o.index]??'—')],['Position σ (x, y)',s?`${fmt(s.x,1)}, ${fmt(s.y,1)}`:'unavailable'],['Mass σ',s?fmt(s.mass,0):'unavailable'],['Disclosed match',!actual?'no disclosed map':match?`#${match.index} · center ${fmt(match.distance,1)} off · radius ${sgn(p.radius-match.actual.radius)}`:'none within 80 units']]};
    }
    if(o.kind==='actual'){
      const p=actual?.[o.index];if(!p)return null;
      const match=planetMatches(b.planets||[],actual).find(m=>m.index===o.index);
      return {title:`Disclosed planet #${o.index}`,rows:[['Center',`(${fmt(p.x,1)}, ${fmt(p.y,1)})`],['Radius',fmt(p.radius,1)],['Mass',fmt(p.mass,0)],['Estimated match',match?`#${(b.planets||[]).indexOf(match.estimated)} · center ${fmt(match.distance,1)} off · radius ${sgn(match.estimated.radius-p.radius)}`:'not estimated']]};
    }
    if(o.kind==='player'){
      const p=bot.player_positions?.[o.id];if(!p)return null;
      const flying=(bot.in_flight||[]).filter(s=>String(s.player_id)===String(o.id)).map(s=>`v${fmt(s.velocity,0)} @ ${fmt(s.angle_degrees,0)}°`);
      return {title:`Player ${o.id}${String(bot.own_id)===String(o.id)?' (this bot)':''}`,rows:[['Last known position',`(${fmt(p.x,1)}, ${fmt(p.y,1)})`],['Shots in flight',flying.join(', ')||'none']]};
    }
    if(o.kind==='contact'){
      const c=b.contact_points?.[o.index];if(!c)return null;
      const e=nearestSurface(c,b.planets),a=nearestSurface(c,actual);
      return {title:`Accepted contact #${o.index}`,rows:[['Position',`(${fmt(c.x,1)}, ${fmt(c.y,1)})`],['Nearest estimated planet',e?`#${e.index} · surface residual ${fmt(e.residual,2)}`:'none'],['Nearest disclosed planet',a?`#${a.index} · surface residual ${fmt(a.residual,2)}`:'unavailable']]};
    }
    if(o.kind==='shot'){
      const e=o.event,pts=e.points||[],end=pts.at(-1);if(!end)return null;
      const est=nearestSurface(end,b.planets),act=nearestSurface(end,actual);
      return {title:`Trajectory · event #${o.sequence}`,rows:[['Player',String(e.player_id)],['Shot',`v${fmt(e.velocity,0)} at ${fmt(e.angle,1)}°`],['Samples',String(pts.length)],['Endpoint',`(${fmt(end.x,1)}, ${fmt(end.y,1)})`],['Nearest estimated planet',est?`#${est.index} · surface residual ${fmt(est.residual,2)}`:'none'],['Nearest disclosed planet',act?`#${act.index} · surface residual ${fmt(act.residual,2)}${act.residual<1?' · on surface (likely impact)':''}`:'unavailable'],['Belief snapshot',`#${snapshot.diagnostic_sequence??'unavailable'}${d?.map_updating?' · map update pending':''}`]]};
    }
    if(o.kind==='point')return {title:'Map point',rows:pointRows(o.x,o.y)};
    return null;
  }
  function renderSelection(){
    const el=$('selection'),info=sel&&snapshot?describe(sel):null;
    if(!info){kv(el,[[sel?'The selected object is not in this snapshot.':'Hover the map for a summary; click a planet, player, contact or trajectory to pin it.',null]]);return;}
    kv(el,[[info.title,null],...info.rows]);
  }
  function drawSelection(){
    if(!sel||!snapshot)return;
    const b=snapshot.diagnostic?.map_belief||{},bot=snapshot.diagnostic?.bot_state||{};
    const ring=(x,y,r)=>{ctx.beginPath();ctx.arc(x,y,r,0,Math.PI*2);ctx.strokeStyle='#ffe27a';ctx.lineWidth=3;ctx.stroke();};
    if(sel.kind==='planet'){const p=b.planets?.[sel.index];if(p)ring(p.x,p.y,p.radius+5);}
    else if(sel.kind==='actual'){const p=snapshot.actual_planets?.[sel.index];if(p)ring(p.x,p.y,p.radius+5);}
    else if(sel.kind==='player'){const p=bot.player_positions?.[sel.id];if(p)ring(p.x,p.y,13);}
    else if(sel.kind==='contact'){const p=b.contact_points?.[sel.index];if(p)ring(p.x,p.y,8);}
    else if(sel.kind==='point')ring(sel.x,sel.y,5);
  }
  function renderPlanetList(){
    const el=$('planetList'),b=snapshot?.diagnostic?.map_belief;el.replaceChildren();
    if(!b){el.textContent='No belief snapshot at this point.';return;}
    const planets=b.planets||[];if(!planets.length){el.textContent='No planets estimated yet.';return;}
    const actual=snapshot.actual_planets,matches=planetMatches(planets,actual||[]);
    const table=document.createElement('table');table.className='table';
    const row=(cells,cls)=>{const tr=document.createElement('tr');if(cls)tr.className=cls;for(const c of cells){const td=document.createElement(cls==='head'?'th':'td');td.textContent=c;tr.append(td);}return tr;};
    table.append(row(['#','center','r','mass','hits','σ pos',...(actual?['Δ oracle']:[])],'head'));
    planets.forEach((p,i)=>{
      const s=planetSigma(i),m=matches.find(m=>m.estimated===p);
      const tr=row([String(i),`${fmt(p.x,0)}, ${fmt(p.y,0)}`,fmt(p.radius,1),fmt(p.mass,0),String(b.planet_contact_counts?.[i]??'—'),s?fmt(s.position,1):'—',...(actual?[m?fmt(m.distance,1):'—']:[])],`row${sel?.kind==='planet'&&sel.index===i?' pinned':''}`);
      tr.addEventListener('click',()=>{sel={kind:'planet',index:i};render();});
      table.append(tr);
    });
    el.append(table);
  }
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
  loadCaptures().catch(e=>setStatus(e.message));
})();
