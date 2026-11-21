(() => {
  const W=1885.618,H=1060.66;
  const $=id=>document.getElementById(id);
  const canvas=$('map'),ctx=canvas.getContext('2d');
  let captureId=null, decisions=[], snapshot=null, simulations=[],timeline=[],sel=null,selScope=null,snapshotRequest=0;
  let position='none',layer='gravity',corrections=true,botNames=new Map();
  const GRID_STEP=3,LAYER_NAMES={gravity:'Gravity belief',uncertainty:'Fit uncertainty',evidence:'Observation coverage',disagreement:'Rival-model disagreement',error:'Belief error'};
  const STATUS_LABEL={unknown:'no comparable belief',ok:'close match',location:'center off',size:'size off',both:'center and size off',missing:'missing'},LOCATION_TOL=10,SIZE_TOL=5,STATUS_COLORS={unknown:'#60dcff',ok:'#60dcff',location:'#ff9d4d',size:'#e8d96d',both:'#ff7a5c',missing:'#ff4d6a'};
  let view={x:0,y:0,width:W,height:H}, viewDrag=null, panned=false;
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
    for(const c of data.captures){const option=document.createElement('option');option.value=c.id;const date=new Date(c.started_at_unix_ms).toLocaleString();const kind=c.metadata?.host==='synthetic-scenario'?'scenario':'live',revision=c.metadata?.build?.git_revision,older=kind==='scenario'&&data.captures.some(o=>o.metadata?.host==='synthetic-scenario'&&o.metadata?.client_name===c.metadata?.client_name&&o.id>c.id);option.textContent=`#${c.id} · ${kind}${older?' (older revision)':''} · ${c.metadata?.client_name||'capture'}${revision?` · ${revision.slice(0,8)}`:''} · ${date}`;select.append(option);}
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
  async function loadSnapshot(){
    const request=++snapshotRequest,requestCapture=captureId,requestDecision=position,requestView={...view};
    setStatus('Loading captured state…');simulations=[];
    try{const step=gridStepForView(),params=`id=${requestCapture}&seq=${requestDecision}&step=${step}&field=total&x=${requestView.x}&y=${requestView.y}&width=${requestView.width}&height=${requestView.height}`;const result=await api(`/api/snapshot?${params}`);if(request!==snapshotRequest||requestCapture!==captureId||requestDecision!==position||requestView.x!==view.x||requestView.y!==view.y||requestView.width!==view.width||requestView.height!==view.height)return;snapshot=result;const scope=`${captureId}|${requestDecision}`;if(scope!==selScope){selScope=scope;sel=snapshot.selected_shot?{kind:'shot',...snapshot.selected_shot}:null;degrade=null;loadReview();}const g=snapshot.field_grid;g.samples=g.samples.map(s=>{const pair=(x,y)=>Number.isFinite(x)&&Number.isFinite(y)?{x,y}:null,vector=pair(s[2],s[3]),total=pair(s[4],s[5]),physical=pair(s[6],s[7]),residual=pair(s[8],s[9]),oracle=pair(s[10],s[11]),delta=total&&oracle?{x:total.x-oracle.x,y:total.y-oracle.y}:null;return {x:s[0],y:s[1],vector,total,physical,residual,oracle,delta,sigma:s[12],supported:s[13],disagreement:s[14],gravity:total?Math.hypot(total.x,total.y):null,actual_gravity:oracle?Math.hypot(oracle.x,oracle.y):null};});const c=currentDecision().choice||{};$('velocity').value=c.velocity||8;$('angle').value=c.angle_degrees||0;setStatus(step===GRID_STEP?'':`Showing ${step}-unit samples to fit this viewport`);render();}
    catch(e){setStatus(e.message);}
  }
  async function loadTimeline(){try{const id=captureId,data=await api(`/api/timeline?id=${id}`);if(id!==captureId)return;timeline=data.timeline;drawScrub();}catch(e){setStatus(e.message);}}
  function currentDecision(){return decisions.find(d=>d.sequence===selected())||{};}
  function sampleCountForStep(step){const startX=Math.floor(view.x/step)*step,startY=Math.floor(view.y/step)*step,endX=Math.ceil(view.x+view.width),endY=Math.ceil(view.y+view.height),columns=Math.floor((endX-startX)/step)+1,rows=Math.floor((endY-startY)/step)+1;return columns*rows;}
  function gridStepForView(){return [1,2,3,5,10,25].find(step=>step>=GRID_STEP&&sampleCountForStep(step)<=300000)||25;}
  function gridAt(x,y){const g=snapshot?.field_grid;if(!g)return null;const col=Math.max(0,Math.min(g.columns-1,Math.round((x-g.origin_x)/g.step))),row=Math.max(0,Math.min(g.rows-1,Math.round((y-g.origin_y)/g.step)));return g.samples[row*g.columns+col]||null;}
  // The Error layer needs a truth map; without one the Gravity layer is shown instead.
  function shownLayer(){return layer==='error'&&!snapshot?.actual_planets?'gravity':layer;}
  function layerVector(p){
    const base=corrections?p?.total:p?.physical;
    if(shownLayer()==='error')return base&&p?.oracle?{x:base.x-p.oracle.x,y:base.y-p.oracle.y}:null;
    return base||null;
  }
  function heatValue(p){const l=shownLayer();if(l==='evidence')return p.supported?1:0;if(l==='uncertainty')return p.sigma;if(l==='disagreement')return p.disagreement;const v=layerVector(p);return v?Math.hypot(v.x,v.y):null;}
  function heatColor(t){const hue=240-240*Math.max(0,Math.min(1,t));return `hsla(${hue},85%,56%,.52)`;}
  function render(){
    const dpr=window.devicePixelRatio||1,rect=canvas.getBoundingClientRect();if(rect.width){canvas.width=Math.round(rect.width*dpr);canvas.height=Math.round(rect.width*H/W*dpr);}const sx=canvas.width/view.width,sy=canvas.height/view.height;ctx.setTransform(sx,0,0,sy,-view.x*sx,-view.y*sy);ctx.fillStyle='#080d13';ctx.fillRect(view.x,view.y,view.width,view.height);
    if(!snapshot){return;}
    const hasDecision=snapshot.selection_kind==='decision';for(const id of ['simulate','compare','audit'])$(id).disabled=!hasDecision;$('decisionPanel').hidden=!hasDecision;
    const samples=snapshot.field_grid?.samples||[],l=shownLayer();
    const values=samples.map(heatValue).filter(Number.isFinite).sort((a,b)=>a-b);
    const quantile=p=>values.length?values[Math.min(values.length-1,Math.floor((values.length-1)*p))]:null;
    const rawLo=quantile(.05),rawHi=quantile(.95);let lo=Number.isFinite(rawLo)?rawLo:0,hi=Number.isFinite(rawHi)?rawHi:1;if(hi<=lo)hi=lo+1;
    // Error magnitudes span orders of magnitude (1/r² near planets), so that layer is coloured on a log scale.
    const tx=raw=>l==='error'?Math.log10(Math.max(raw,1e-9)):raw,span=(tx(hi)-tx(lo))||1;
    const unit=l==='disagreement'?'gravity spread between the no-source and source models':l==='uncertainty'?'field σ from fit covariance':l==='evidence'?'residual support (0 or 1)':l==='error'?'|belief − truth gravity|':'|gravity|';
    $('scaleMin').textContent=Number.isFinite(rawLo)?fmt(rawLo,4):'no data';$('scaleMax').textContent=Number.isFinite(rawHi)?fmt(rawHi,4):'no data';
    $('scaleBar').style.background=l==='evidence'?'linear-gradient(90deg,#080d13,#2aaa8c)':'linear-gradient(90deg,hsl(240 85% 56%),hsl(180 85% 56%),hsl(120 85% 56%),hsl(60 85% 56%),hsl(0 85% 56%))';
    $('scaleBar').title=`Colors span the 5th–95th percentile${l==='error'?' on a log scale':''} · ${fmt(rawLo,6)} to ${fmt(rawHi,6)} · ${unit}`;
    for(const p of samples){const raw=heatValue(p);if(!Number.isFinite(raw))continue;const t=Math.max(0,Math.min(1,(tx(raw)-tx(lo))/span));ctx.fillStyle=l==='evidence'?(p.supported?'rgba(42,170,140,.35)':'rgba(0,0,0,.22)'):heatColor(t);ctx.fillRect(p.x,p.y,snapshot.field_grid.step,snapshot.field_grid.step);}
    if($('arrows').checked&&(l==='gravity'||l==='error')){
      const spacing=36*dpr;ctx.save();ctx.setTransform(1,0,0,1,0,0);ctx.strokeStyle='rgba(255,255,255,.85)';ctx.lineWidth=1.3*dpr;ctx.lineCap='round';
      for(let py=spacing/2;py<canvas.height;py+=spacing)for(let px=spacing/2;px<canvas.width;px+=spacing){
        const v=layerVector(gridAt(view.x+px/sx,view.y+py/sy)),m=v?Math.hypot(v.x,v.y):0;if(!(m>0))continue;
        const ux=v.x/m,uy=v.y/m,half=6*dpr,tx2=px+ux*half,ty2=py+uy*half,head=3.5*dpr;
        ctx.beginPath();ctx.moveTo(px-ux*half,py-uy*half);ctx.lineTo(tx2,ty2);ctx.moveTo(tx2-ux*head-uy*head*.6,ty2-uy*head+ux*head*.6);ctx.lineTo(tx2,ty2);ctx.lineTo(tx2-ux*head+uy*head*.6,ty2-uy*head-ux*head*.6);ctx.stroke();
      }
      ctx.restore();
    }
    if(!values.length){ctx.save();ctx.setTransform(1,0,0,1,0,0);ctx.fillStyle='#ffc66d';ctx.font='14px system-ui';ctx.fillText(l==='uncertainty'?'Uncertainty unavailable: this snapshot has no fit covariance (rank-deficient, too few paths, or not converged).':l==='disagreement'?'Model disagreement unavailable: no validated field-only source in this snapshot, so there is no competing model to compare.':l==='evidence'?'No residual evidence in this snapshot.':'No field values for this layer in this snapshot.',16,28);ctx.restore();}
    if($('pathResiduals').checked){const residuals=(snapshot.events||[]).flatMap(e=>e.residual_samples||[]),mags=residuals.map(r=>Math.hypot(r[2],r[3])).sort((a,b)=>a-b),cap=mags.length?mags[Math.floor((mags.length-1)*.95)]:null;$('pathScale').style.display=residuals.length?'grid':'none';$('pathMax').textContent=Number.isFinite(cap)?fmt(cap,4):'no data';$('pathScaleBar').title=`Path residual magnitude, colors clip at the 95th percentile (${fmt(cap,6)})`;for(const r of residuals){const t=Math.max(0,Math.min(1,Math.hypot(r[2],r[3])/(cap||1)));ctx.beginPath();ctx.arc(r[0],r[1],1.7,0,Math.PI*2);ctx.fillStyle=heatColor(t);ctx.fill();}}else $('pathScale').style.display='none';
    if($('paths').checked){const paths=(snapshot.events||[]).filter(e=>e.record?.event?.type==='shot_fin');for(const item of paths){const e=item.record.event;if(!e.points?.length)continue;ctx.beginPath();ctx.moveTo(e.points[0].x,e.points[0].y);for(const p of e.points)ctx.lineTo(p.x,p.y);ctx.strokeStyle=colors[e.player_id%colors.length]+(item.label==='trial'?'ee':item.label?.startsWith('training')?'33':newerThanBelief(item)?'99':'55');ctx.lineWidth=item.label==='trial'?2.6:item.label?1:1.2;ctx.setLineDash(newerThanBelief(item)?[5,4]:[]);ctx.stroke();ctx.setLineDash([]);const end=e.points.at(-1);ctx.beginPath();ctx.arc(end.x,end.y,2.8,0,Math.PI*2);ctx.fillStyle=colors[e.player_id%colors.length]+'bb';ctx.fill();if(item.label){ctx.fillStyle='#cfd8e6';ctx.font='12px system-ui';ctx.fillText(item.label,e.points[0].x+8,e.points[0].y+16);}}}
    if($('beliefPlanets').checked){for(const p of snapshot.diagnostic?.map_belief?.planets||[])circle(p,'#ffc45b',true);}
    if($('actualPlanets').checked){const status=disclosedStatuses();(snapshot.actual_planets||[]).forEach((p,i)=>{const s=status[i]?.status||'ok';circle(p,STATUS_COLORS[s],false);if(s!=='ok'&&s!=='unknown'){ctx.fillStyle=STATUS_COLORS[s];ctx.font='12px system-ui';ctx.fillText(`#${i}`,p.x+p.radius*.7,p.y-p.radius*.7-3);}});}
    drawHypothesis();
    if($('spawns').checked){
      const spawns=snapshot.diagnostic?.bot_state?.observed_spawn_locations||[];
      ctx.save();ctx.strokeStyle='#9ad7ff';ctx.lineWidth=2.2;
      for(const p of spawns){ctx.strokeRect(p.x-8,p.y-8,16,16);ctx.beginPath();ctx.moveTo(p.x-8,p.y);ctx.lineTo(p.x+8,p.y);ctx.moveTo(p.x,p.y-8);ctx.lineTo(p.x,p.y+8);ctx.stroke();}
      if(!spawns.length){ctx.setTransform(1,0,0,1,0,0);ctx.fillStyle='#9ad7ff';ctx.font='13px system-ui';ctx.fillText('No spawn samples recorded in this snapshot.',16,canvas.height/(window.devicePixelRatio||1)-16);}
      ctx.restore();
    }
    if($('contacts').checked){ctx.fillStyle='#c58cff';for(const p of snapshot.diagnostic?.map_belief?.contact_points||[]){ctx.beginPath();ctx.arc(p.x,p.y,3,0,Math.PI*2);ctx.fill();}}
    for(const [id,list] of Object.entries(snapshot.world?.ghosts||{}))list.forEach((g,k)=>{ctx.beginPath();ctx.arc(g.x,g.y,6,0,Math.PI*2);ctx.globalAlpha=.12+.1*k;ctx.fillStyle=colors[Number(id)%colors.length];ctx.fill();ctx.globalAlpha=1;});
    const players=snapshot.world?.players||{};for(const [id,p] of Object.entries(players)){ctx.beginPath();ctx.arc(p.x,p.y,id===String(snapshot.world?.own_id)?8:6,0,Math.PI*2);ctx.fillStyle=id===String(snapshot.world?.own_id)?'#f4f7ff':colors[Number(id)%colors.length];ctx.fill();ctx.strokeStyle='#080d13';ctx.lineWidth=2;ctx.stroke();ctx.fillStyle='#fff';ctx.font='12px system-ui';ctx.fillText(pname(id),p.x+8,p.y-8);}
    for(const sim of simulations){const pts=sim.data.points||[];if(!pts.length)continue;ctx.beginPath();ctx.moveTo(pts[0].x,pts[0].y);for(const p of pts)ctx.lineTo(p.x,p.y);ctx.strokeStyle=sim.color;ctx.lineWidth=3;ctx.stroke();}
    if(sel?.kind==='shot'&&sel.event?.points?.length){const pts=sel.event.points;ctx.beginPath();ctx.moveTo(pts[0].x,pts[0].y);for(const p of pts)ctx.lineTo(p.x,p.y);ctx.strokeStyle='#fff2a8';ctx.lineWidth=3;ctx.stroke();const end=pts.at(-1);ctx.beginPath();ctx.arc(end.x,end.y,6,0,Math.PI*2);ctx.strokeStyle='#fff2a8';ctx.lineWidth=2;ctx.stroke();}
    drawReview();
    drawDegrade();
    drawSelection();
    const decision=currentDecision(),choice=decision.choice||{};$('decisionLabel').textContent=snapshot.selection_kind==='decision'?`Decision #${decision.sequence} · ${decision.bot_name} · ${new Date(decision.timestamp_unix_ms||0).toLocaleTimeString()} · map updating: ${decision.map_updating?'yes':'no'}`:snapshot.selection_kind==='timeline_event'?`Event #${snapshot.sequence} · belief as of decision snapshot #${snapshot.diagnostic_sequence??'unavailable'}${newerPathsNote()}`:`Latest state · events through #${snapshot.sequence} · belief snapshot #${snapshot.diagnostic_sequence??'unavailable'}${newerPathsNote()}`;
    renderMetrics(decision);renderChoice(choice);renderDisclosedList();renderPlanetList();renderSelection();renderDegrade();syncLayerButtons();
  }
  // A path the recorded belief has not seen: it ended after the belief's snapshot (or no belief exists yet).
  function newerPathsNote(){const n=(snapshot.events||[]).filter(item=>item.record?.event?.type==='shot_fin'&&newerThanBelief(item)).length;return n?` · ${n} path${n===1?'':'s'} newer than the belief (dashed)`:'';}
  const newerThanBelief=item=>item.sequence>(snapshot.diagnostic_sequence??-1);
  function circle(p,color,fill){ctx.beginPath();ctx.arc(p.x,p.y,Math.max(3,p.radius||3),0,Math.PI*2);ctx.lineWidth=2;ctx.strokeStyle=color;if(fill){ctx.fillStyle=color+'26';ctx.fill();}else ctx.setLineDash([7,5]);ctx.stroke();ctx.setLineDash([]);}
  function kv(el,rows){el.replaceChildren();for(const [a,b,hint] of rows){if(b===null){const h=document.createElement('b');h.className='wide';h.textContent=a;el.append(h);continue;}const x=document.createElement('span');x.textContent=a;if(hint)x.title=hint;const y=document.createElement('b');y.textContent=b;el.append(x,y);}}
  function renderMetrics(d){
    if(!snapshot.diagnostic){kv($('metrics'),[['No belief recorded yet at this point; the first targeting decision has not happened.',null]]);return;}
    const b=snapshot.diagnostic?.map_belief||{},g=snapshot.field_grid||{},cov=b.covariance;
    const sigmas=g.samples?.map(p=>p.sigma).filter(Number.isFinite)||[];const sorted=[...sigmas].sort((a,b)=>a-b);const median=sorted.length?sorted[Math.floor(sorted.length/2)]:null;
    const local=b.local_field?.samples?.length||0;
    const delta=snapshot.field_grid?.samples?.map(p=>{const base=corrections?p.total:p.physical;return base&&p.oracle?{x:base.x-p.oracle.x,y:base.y-p.oracle.y}:null;}).filter(v=>v&&Number.isFinite(v.x)&&Number.isFinite(v.y))||[],errors=delta.map(v=>Math.hypot(v.x,v.y)).sort((a,b)=>a-b),fieldRmse=delta.length?Math.sqrt(delta.reduce((sum,v)=>sum+v.x*v.x+v.y*v.y,0)/delta.length):null;
    const matches=planetMatches();const centerErrors=matches.map(m=>m.distance).sort((a,b)=>a-b),radiusErrors=matches.map(m=>Math.abs(m.radiusError)).sort((a,b)=>a-b);
    const stale=d.map_updating===true;
    const oracle=Boolean(snapshot.actual_planets);
    kv($('metrics'),[['Map state',stale?'stale · fitting newer shots':'latest available'],['Estimated planets',oracle?`${b.planets?.length??0} of ${snapshot.actual_planets.length} truth`:String(b.planets?.length??0)],...(oracle?[['Truth planets missing',String(snapshot.planet_match?.missing_actual?.length??'—')],['Estimates matching nothing',String(snapshot.planet_match?.unmatched_estimated?.length??'—')],['Median center / radius error',matches.length?`${fmt(middle(centerErrors))} / ${fmt(middle(radiusErrors))} units`:'—'],['Gravity error · RMS / p95 (viewport)',errors.length?`${fmt(fieldRmse)} / ${fmt(errors[Math.floor(errors.length*.95)])}`:'—']]:[]),['Fit trajectory RMSE',Number.isFinite(b.model_position_rmse)?`${fmt(b.model_position_rmse)} units`:'—'],['Accepted impact points',String(b.contact_points?.length??0)],['Local residual samples',String(local)],['Fit converged',b.fit_converged===null||b.fit_converged===undefined?'—':b.fit_converged?'yes':'no'],['Ambiguous associations',String(b.uncertain_associations??0)],['Fit covariance',g.has_covariance?'available':'unavailable']]);
  }
  function renderChoice(c){kv($('choice'),[['Reason',String(c.reason||'—')],['Selected velocity',String(c.velocity??'—')],['Selected angle',`${c.angle_degrees??'—'}°`],['Predicted victim',String(c.predicted_victim??'—')],['Predicted center miss',`${fmt(c.miss_distance)} units`],['Alternative model agrees',String(c.alternative_agrees??'untested')],['Field hypothesis used',String(c.used_field_hypothesis??false)],['Covered path fraction',fmt(c.covered_fraction,3)],...predictiveRows(),...mapAgeRows()]);}
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
      if(recorded?.velocity){const model=recorded.used_field_hypothesis?'hypothesis':'belief';await sim(recorded,model,'#8ee5a4','Recorded shot, captured belief');await sim(recorded,'actual','#ff8d68','Recorded shot, truth map');}
      const oracle=result.actual_map_choice_current_policy;if(oracle?.velocity)await sim(oracle,'actual','#ffe27a','Current policy, truth map');
      const choiceText=c=>c?.velocity?`v${c.velocity} @ ${c.angle_degrees}° · ${c.reason||'shot'}${c.predicted_victim!=null?` · victim ${c.predicted_victim}`:''}`:'no shot',endText=r=>`${r.termination.kind.replaceAll('_',' ')}${r.termination.player_id!==undefined?` · player ${r.termination.player_id}`:''}`;kv($('comparison'),[['Recorded choice',choiceText(recorded)],['Actual-map choice (current policy)',choiceText(oracle)],...runs.map(r=>[r.label,endText(r)]),...(result.note?[[result.note,null]]:[])]);
      setStatus('Green: belief · orange: recorded shot on actual map · yellow: current policy on actual map');render();
    }catch(e){$('comparison').textContent=e.message;setStatus(e.message);}}
  async function audit(){const d=currentDecision();try{setStatus('Replaying observed paths…');const result=await api(`/api/physics-audit?id=${captureId}&seq=${d.sequence}`);simulations=[{data:{points:result.worst_observed_points},color:'#d7e2ff'},{data:{points:result.worst_rust_points},color:'#ff4da6'}];kv($('comparison'),[['Audited shots',String(result.audited_shots)],['Planets in map',String(result.map_planets)],['Timed samples',String(result.timed_samples)],['Coordinate RMSE',fmt(result.coordinate_rmse)],['Median / p95 / max position error',`${fmt(result.median_position_error)} / ${fmt(result.p95_position_error)} / ${fmt(result.max_position_error)}`],['Worst shot',(w=>w?`event #${w.shot_sequence} · player ${w.player_id} · v${fmt(w.velocity,0)} @ ${fmt(w.angle,1)}° · RMSE ${fmt(w.coordinate_rmse)} · max ${fmt(w.max_position_error)} · Rust: ${String(w.rust_termination?.kind||'—').replaceAll('_',' ')}`:'—')(result.worst_shot)],...(result.note?[[result.note,null]]:[])]);setStatus(`Rust path replay: coordinate RMSE ${fmt(result.coordinate_rmse)} units · worst shot overlaid`);render();}catch(e){$('comparison').textContent=e.message;setStatus(e.message);}}
  $('capture').addEventListener('change',()=>{showCaptureView();resetView();loadDecisions();});$('bot').addEventListener('change',drawScrub);$('refresh').addEventListener('click',()=>{if(!$('overview').hidden)loadOverview();else loadDecisions($('bot').value);});for(const id of ['paths','pathResiduals','beliefPlanets','actualPlanets','contacts','spawns'])$(id).addEventListener('change',render);$('simulate').addEventListener('click',simulate);$('compare').addEventListener('click',compare);$('audit').addEventListener('click',audit);window.addEventListener('resize',()=>{render();drawScrub();});
  let viewportTimer=null;function refreshViewport(){clearTimeout(viewportTimer);viewportTimer=setTimeout(loadSnapshot,120);}
  function zoomAt(factor,px=W/2,py=H/2){const width=Math.max(40,Math.min(W,view.width*factor)),height=width*H/W,cx=view.x+px/W*view.width,cy=view.y+py/H*view.height;view={x:Math.max(0,Math.min(W-width,cx-px/W*width)),y:Math.max(0,Math.min(H-height,cy-py/H*height)),width,height};refreshViewport();}
  function resetView(){view={x:0,y:0,width:W,height:H};loadSnapshot();}
  $('zoomIn').addEventListener('click',()=>zoomAt(.6));$('zoomOut').addEventListener('click',()=>zoomAt(1.67));$('resetView').addEventListener('click',resetView);
  canvas.addEventListener('dblclick',resetView);
  canvas.addEventListener('pointerdown',e=>{panned=false;if(!e.shiftKey&&e.button!==1)return;const rect=canvas.getBoundingClientRect();viewDrag={clientX:e.clientX,clientY:e.clientY,rect,view:{...view}};canvas.setPointerCapture(e.pointerId);});
  canvas.addEventListener('pointermove',e=>{if(!viewDrag)return;const dx=(e.clientX-viewDrag.clientX)/viewDrag.rect.width*viewDrag.view.width,dy=(e.clientY-viewDrag.clientY)/viewDrag.rect.height*viewDrag.view.height;view={...viewDrag.view,x:Math.max(0,Math.min(W-viewDrag.view.width,viewDrag.view.x-dx)),y:Math.max(0,Math.min(H-viewDrag.view.height,viewDrag.view.y-dy))};render();});canvas.addEventListener('pointerup',()=>{if(viewDrag){viewDrag=null;panned=true;refreshViewport();}});
  // ---- Captures overview: the landing table, one row per capture, sortable, with global metrics ----
  let overviewRows=[],overviewSort={key:'id',dir:-1};
  const overviewValue={
    id:r=>r.id,kind:r=>r.kind,name:r=>r.name,decisions:r=>r.decisions,shots:r=>r.shots,
    estimated:r=>r.last?.planets,disclosed:r=>r.last?.accuracy?.actual_planets,missing:r=>r.last?.accuracy?.missing_planets,
    missingShare:r=>r.last?.accuracy?r.last.accuracy.missing_planets/Math.max(1,r.last.accuracy.actual_planets):null,
    centerError:r=>r.last?.accuracy?.median_center_error,rmse:r=>r.last?.fit_rmse,converged:r=>r.last?.fit_converged==null?null:r.last.fit_converged?1:0,
  };
  const overviewColumns=[['id','#'],['kind','kind'],['name','name'],['decisions','decisions'],['shots','shots'],['estimated','est. planets'],['disclosed','truth'],['missing','missing'],['missingShare','missing %'],['centerError','median center err'],['rmse','fit RMSE'],['converged','converged']];
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
      ['Median final fit RMSE',units(middle(rows.map(overviewValue.rmse)))],
    ]);
    const table=$('overviewTable');table.replaceChildren();
    const head=document.createElement('tr');
    for(const [key,label] of overviewColumns){const th=document.createElement('th');th.textContent=label+(overviewSort.key===key?(overviewSort.dir>0?' ▲':' ▼'):'');th.addEventListener('click',()=>{overviewSort={key,dir:overviewSort.key===key?-overviewSort.dir:-1};renderOverview();});head.append(th);}
    table.append(head);
    for(const r of rows){
      const a=r.last?.accuracy,tr=document.createElement('tr');tr.className='row';
      tr.title=`${new Date(r.started_at_unix_ms).toLocaleString()}${r.revision?` · ${r.revision.slice(0,8)}`:''}${r.last?.stage?` · ${r.last.stage}`:''}`;
      const cells=[r.id,r.kind,r.name,r.decisions,r.shots,r.last?.planets??'—',a?.actual_planets??'—',a?.missing_planets??'—',a?pct(a.missing_planets/Math.max(1,a.actual_planets)):'—',a&&a.median_center_error!=null?fmt(a.median_center_error,1):'—',Number.isFinite(r.last?.fit_rmse)?fmt(r.last.fit_rmse,1):'—',r.last?.fit_converged==null?'—':r.last.fit_converged?'yes':'no'];
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
  const REVIEW_COLORS={observed:'#ffffff',physical:'#9aa7ff',local:'#8ee5a4',hypothesis:'#ff9bf0',actual:'#ff8d68'};
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
        const n=document.createElement('div');n.className='note';n.textContent=`Baseline RMSE ${fmt(r.baseline_rmse,1)}; correcting every listed planet at once gives ${fmt(r.all_repaired_rmse,1)}. ${r.note} The recorded residual correction was fitted to the old planets and can over-compensate once they are corrected.`;
        repairs.append(t,n);
      }else if(review.repairs?.error){repairs.textContent=review.repairs.error;}
      else{const b=document.createElement('button');b.textContent='Test repairs with the truth map';b.title='Change one planet of the belief to the truth one, re-simulate, and rank by how much the path improves';b.addEventListener('click',()=>{b.disabled=true;b.textContent='Simulating…';loadReview(true);});repairs.append(b);}
    }
  }
  function drawReview(){
    if(!review?.shot||snapshot?.selection_kind!=='decision'||String(review.decision.sequence)!==position)return;
    const line=(points,color,width,dash)=>{if(!points?.length)return;ctx.beginPath();ctx.moveTo(points[0].x,points[0].y);for(const p of points)ctx.lineTo(p.x,p.y);ctx.strokeStyle=color;ctx.lineWidth=width;ctx.setLineDash(dash);ctx.stroke();ctx.setLineDash([]);};
    for(const v of review.variants)if(v.available&&reviewVisible.has(v.key))line(v.points,REVIEW_COLORS[v.key],v.as_used?3:2,[7,5]);
    if(reviewVisible.has('observed'))line(review.shot.points,REVIEW_COLORS.observed,3,[]);
    const div=review.divergence;if(div&&reviewVisible.has((review.variants.find(v=>v.as_used)||{}).key)){ctx.beginPath();ctx.arc(div.point.x,div.point.y,9,0,Math.PI*2);ctx.strokeStyle='#ffe27a';ctx.lineWidth=2.5;ctx.stroke();}
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
  function drawDegrade(){
    const r=degrade?.rows?.[degrade.selected];if(!r?.claimed||snapshot?.selection_kind!=='decision')return;
    const line=(points,color,dash)=>{if(!points?.length)return;ctx.beginPath();ctx.moveTo(points[0].x,points[0].y);for(const p of points)ctx.lineTo(p.x,p.y);ctx.strokeStyle=color;ctx.lineWidth=3;ctx.setLineDash(dash);ctx.stroke();ctx.setLineDash([]);};
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

  // ---- Layers: Gravity, Uncertainty, Evidence and Error are self-contained modes; the client derives every vector from the per-sample fields ----
  function syncLayerButtons(){
    const hasOracle=Boolean(snapshot?.actual_planets),shown=shownLayer(),samples=snapshot?.field_grid?.samples||[];
    const missing={
      uncertainty:samples.some(p=>Number.isFinite(p.sigma))?null:'no fit covariance in this snapshot (rank-deficient, too few paths, or not emitted)',
      evidence:samples.some(p=>p.supported)?null:'no recorded path supports a local correction here',
      disagreement:samples.some(p=>Number.isFinite(p.disagreement))?null:'no validated field-only source, so there is no rival model',
      error:hasOracle?null:'needs a truth map; this capture has none at this point'};
    for(const b of document.querySelectorAll('.layer')){
      const name=b.dataset.layer;if(b.dataset.help===undefined)b.dataset.help=b.title;
      b.classList.toggle('active',name===shown);b.classList.toggle('unavailable',Boolean(missing[name]));
      b.title=missing[name]?`Unavailable: ${missing[name]}`:b.dataset.help;
    }
    $('errorLayer').disabled=!hasOracle;
    $('actualPlanetsLabel').style.display=$('actualLegend').style.display=$('disclosedPanel').style.display=hasOracle?'':'none';
    $('correctionsLabel').style.display=shown==='gravity'||shown==='error'?'':'none';
    $('arrowsLabel').style.display=shown==='gravity'||shown==='error'?'':'none';
  }
  for(const b of document.querySelectorAll('.layer'))b.addEventListener('click',()=>{layer=b.dataset.layer;render();});
  $('arrows').addEventListener('change',render);
  $('corrections').addEventListener('change',()=>{corrections=$('corrections').checked;render();});
  syncLayerButtons();
  // ---- Time navigation: one scrubber over every event and decision in the capture ----
  const scrub=$('scrub'),scrubCtx=scrub.getContext('2d'),SCRUB_H=104,PAD_L=8,PAD_R=8,TREND_B=56;
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
    // Trend strip: estimated planet count and fit RMSE at each (visible) decision.
    const points=timeline.map((item,i)=>({item,i,d:item.kind==='decision'?decisionAt(item.sequence):null})).filter(p=>p.d&&botVisible(p.d));
    const withTruth=points.filter(p=>p.d.accuracy),maxPlanets=Math.max(1,...points.map(p=>Math.max(p.d.map_planets||0,p.d.accuracy?.actual_planets||0))),maxRmse=Math.max(1e-9,...points.map(p=>p.d.fit_rmse||0));
    const yPlanets=v=>TREND_B-2-v/maxPlanets*(TREND_B-18),yRmse=v=>TREND_B-2-v/maxRmse*(TREND_B-18);
    c.lineWidth=1.5;
    c.strokeStyle='#ffc45b';c.beginPath();points.forEach((p,k)=>{const x=scrubX(p.i,w),y=yPlanets(p.d.map_planets||0);if(k)c.lineTo(x,yPlanets(points[k-1].d.map_planets||0));k?c.lineTo(x,y):c.moveTo(x,y);});c.stroke();
    if(withTruth.length){c.strokeStyle='#ff4d6a';c.beginPath();withTruth.forEach((p,k)=>{const x=scrubX(p.i,w),y=yPlanets(p.d.accuracy.missing_planets);if(k)c.lineTo(x,yPlanets(withTruth[k-1].d.accuracy.missing_planets));k?c.lineTo(x,y):c.moveTo(x,y);});c.stroke();}
    const scored=points.filter(p=>Number.isFinite(predictive.get(p.d.sequence)?.median_rmse)),maxPred=Math.max(1e-9,...scored.map(p=>predictive.get(p.d.sequence).median_rmse)),yPred=v=>TREND_B-2-v/maxPred*(TREND_B-18);
    if(scored.length){c.strokeStyle='#7de3a1';c.lineWidth=1.5;c.beginPath();scored.forEach((p,k)=>{const x=scrubX(p.i,w),y=yPred(predictive.get(p.d.sequence).median_rmse);k?c.lineTo(x,y):c.moveTo(x,y);});c.stroke();for(const p of scored){c.beginPath();c.arc(scrubX(p.i,w),yPred(predictive.get(p.d.sequence).median_rmse),2,0,Math.PI*2);c.fillStyle='#7de3a1';c.fill();}}
    c.strokeStyle='#6ec8ff';c.beginPath();let pen=false;for(const p of points){if(!Number.isFinite(p.d.fit_rmse)){pen=false;continue;}const x=scrubX(p.i,w),y=yRmse(p.d.fit_rmse);pen?c.lineTo(x,y):c.moveTo(x,y);pen=true;}c.stroke();
    for(const p of points){if(!Number.isFinite(p.d.fit_rmse))continue;c.beginPath();c.arc(scrubX(p.i,w),yRmse(p.d.fit_rmse),2.5,0,Math.PI*2);if(p.d.fit_converged){c.fillStyle='#6ec8ff';c.fill();}else{c.strokeStyle='#6ec8ff';c.lineWidth=1;c.stroke();c.lineWidth=1.5;}}
    c.font='10px ui-monospace,monospace';let lx=PAD_L;for(const [text,color] of [[`estimated planets ≤${maxPlanets}`,'#ffc45b'],...(withTruth.length?[['missing truth planets','#ff4d6a']]:[]),[`fit RMSE ≤${fmt(maxRmse,maxRmse<10?2:0)} (● converged ○ not)`,'#6ec8ff'],...(scored.length?[[`next-${predictiveHorizon}-shot error ≤${fmt(maxPred,0)}`,'#7de3a1']]:[])]){c.fillStyle=color;c.fillText(text,lx,11);lx+=c.measureText(text).width+14;}
    // Event ticks: decisions tall and colored by bot, shot ends medium, map resets red, everything else faint.
    for(let i=0;i<timeline.length;i++){
      const item=timeline[i],x=scrubX(i,w);let top=TREND_B+22,color='#2c3a4d';
      if(item.kind==='decision'){const d=decisionAt(item.sequence);top=TREND_B+4;color=colors[Math.max(0,bots.indexOf(d?.bot_name))%colors.length]+(botVisible(d)?'':'44');}
      else if(item.kind==='shot_fin'){top=TREND_B+14;color='#7a8ea8';}
      else if(item.kind==='planets'){top=4;color='#ff6b6b';}
      c.strokeStyle=color;c.lineWidth=item.kind==='decision'||item.kind==='planets'?2:1;c.beginPath();c.moveTo(x,top);c.lineTo(x,SCRUB_H-6);c.stroke();
    }
    const h=headIndex();if(h>=0){const x=scrubX(h,w);c.strokeStyle='#ffe27a';c.lineWidth=2;c.beginPath();c.moveTo(x,0);c.lineTo(x,SCRUB_H);c.stroke();c.fillStyle='#ffe27a';c.beginPath();c.moveTo(x-5,0);c.lineTo(x+5,0);c.lineTo(x,7);c.fill();}
  }
  function scrubTip(i){
    const item=timeline[i],d=item.kind==='decision'?decisionAt(item.sequence):null;
    const lines=[`#${item.sequence} ${item.kind} · ${item.detail.replace(/player (\d+)/g,(m,id)=>`player ${pname(id)}`)}`,new Date(item.timestamp_unix_ms).toLocaleTimeString()];
    if(d)lines.push(`${d.bot_name} · ${d.map_planets} planets · fit RMSE ${fmt(d.fit_rmse)} · ${d.fit_converged?'converged':'not converged'}`);
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
    const hypothesis=belief.field_hypothesis;if(hypothesis&&Math.hypot(x-hypothesis.x,y-hypothesis.y)<=9+tol)return {kind:'hypothesis'};
    if($('beliefPlanets').checked){const index=nearestPlanetIndex(x,y,tol,belief.planets||[]);if(index>=0)return {kind:'planet',index};}
    if($('actualPlanets').checked){const index=nearestPlanetIndex(x,y,tol,snapshot.actual_planets||[]);if(index>=0)return {kind:'actual',index};}
    if($('paths').checked){
      let best=null;
      for(const item of snapshot.events||[]){const e=item.record?.event;if(e?.type!=='shot_fin')continue;let dist=Infinity;for(let i=1;i<(e.points?.length||0);i++)dist=Math.min(dist,segmentDistance(x,y,e.points[i-1],e.points[i]));if(dist<=tol&&(!best||dist<best.dist))best={dist,sequence:item.sequence,event:e,label:item.label};}
      if(best)return {kind:'shot',sequence:best.sequence,event:best.event,label:best.label};
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
    const scalar=l==='uncertainty'?['Field σ (fit covariance)',Number.isFinite(p?.sigma)?fmt(p.sigma,6):'unavailable','Spread of the gravity belief implied by the planet-fit covariance. Ignores unseen planets and model error.']:l==='evidence'?['Observation coverage',p?.supported?'a recorded path supports a correction here':'no recorded path near here','Teal cells are near recorded paths that fed a local correction.']:l==='disagreement'?['Rival-model disagreement',Number.isFinite(p?.disagreement)?fmt(p.disagreement,6):'unavailable (no validated source)','How differently the no-source and the field-only source models pull here, weighted by how undecided the evidence between them is.']:null;
    const belief=['Belief pull',p?.total?dirText(p.total):'no value here',`Gravity the bot simulates with at this point: fitted planets plus local correction. ${AXES}`];
    const truth=['Truth pull',p?.oracle?dirText(p.oracle):'no truth map',`Gravity of the truth map at this point (the planets the server disclosed; diagnosis only, never fit input). ${AXES}`];
    const error=['Error (belief − truth)',p?.delta?`${dirText(p.delta)}${p.oracle&&Math.hypot(p.oracle.x,p.oracle.y)>0?` · ${fmt(100*Math.hypot(p.delta.x,p.delta.y)/Math.hypot(p.oracle.x,p.oracle.y),0)}% of truth`:''}`:'unavailable','The arrow points to where the belief pulls extra compared with truth; the opposite way means the belief pulls too little.'];
    return [...(scalar?[scalar]:[]),...(l==='error'?[error,belief,truth]:[belief,truth,error]),
      ['  from planets',p?.physical?dirText(p.physical):'no value','The fitted planets alone.'],
      ['  from local correction',p?.residual&&Math.hypot(p.residual.x,p.residual.y)>0?dirText(p.residual):'none here','Observed path residual added on top of the planets; absent where no recorded path supports one.'],
      ['Position',`(${fmt(x,1)}, ${fmt(y,1)})`],['Nearest estimated planet',near({x,y},b.planets)],['Nearest truth planet',snapshot.actual_planets?near({x,y},snapshot.actual_planets):'unavailable']];
  }
  // Returns {title, rows} for a selection, or null when the object is not in this snapshot.
  function describe(o){
    const d=snapshot?.diagnostic,b=d?.map_belief||{},actual=snapshot?.actual_planets;
    if(o.kind==='planet'){
      const p=b.planets?.[o.index];if(!p)return null;
      const s=planetSigma(o.index),match=planetMatches().find(m=>m.estimatedIndex===o.index);
      return {title:`Estimated planet #${o.index}`,rows:[['Center',`(${fmt(p.x,1)}, ${fmt(p.y,1)})`],['Radius',fmt(p.radius,1)],['Mass',fmt(p.mass,0)],['Surface contacts',String(b.planet_contact_counts?.[o.index]??'—')],['Position σ (x, y)',s?`${fmt(s.x,1)}, ${fmt(s.y,1)}`:'unavailable'],['Mass σ',s?fmt(s.mass,0):'unavailable'],['Truth match',!actual?'no truth map':match?`#${match.index} · ${STATUS_LABEL[matchStatus(match)]} · ${matchNumbers(match)}`:'no truth planet within 80 units (unmatched estimate)']]};
    }
    if(o.kind==='actual'){
      const p=actual?.[o.index];if(!p)return null;
      const match=planetMatches().find(m=>m.index===o.index);
      const st=disclosedStatuses()[o.index];
      return {title:`Truth planet #${o.index}`,rows:[['Center',`(${fmt(p.x,1)}, ${fmt(p.y,1)})`],['Radius',fmt(p.radius,1)],['Mass',fmt(p.mass,0)],['Status',`${STATUS_LABEL[st.status]} (center > ${LOCATION_TOL} or radius > ${SIZE_TOL} units off)`],['Estimated match',match?`#${match.estimatedIndex} · ${matchNumbers(match)}`:'no estimate within 80 units']]};
    }
    if(o.kind==='hypothesis'){
      const h=b.field_hypothesis;if(!h)return null;
      return {title:'Field-only source hypothesis',rows:[['Position',`(${fmt(h.x,1)}, ${fmt(h.y,1)})`],['Mass',fmt(h.mass,0)],['Support points',String(h.support?.length??0)],['Continuation RMSE',`${fmt(h.continuation_rmse)} (baseline ${fmt(h.baseline_continuation_rmse)})`],['Meaning','a gravity explanation only, never collision geometry']]};
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
      const e=o.event,pts=e.points||[],end=pts.at(-1);if(!end)return null;
      const est=nearestSurface(end,b.planets),act=nearestSurface(end,actual);
      return {title:`Trajectory${o.label?` (${o.label})`:''} · event #${o.sequence}`,rows:[['Player',pname(e.player_id)],...(o.label?[['Scenario role',o.label]]:[]),['Shot',`v${fmt(e.velocity,0)} at ${fmt(e.angle,1)}°`],['Samples',String(pts.length)],['Endpoint',`(${fmt(end.x,1)}, ${fmt(end.y,1)})`],['Nearest estimated planet',est?`#${est.index} · surface residual ${fmt(est.residual,2)}`:'none'],['Nearest truth planet',act?`#${act.index} · surface residual ${fmt(act.residual,2)}${act.residual<1?' · on surface (likely impact)':''}`:'unavailable'],['Belief snapshot',`#${snapshot.diagnostic_sequence??'unavailable'}${d?.map_updating?' · map update pending':''}`]]};
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
    const b=snapshot.diagnostic?.map_belief||{};
    const ring=(x,y,r)=>{ctx.beginPath();ctx.arc(x,y,r,0,Math.PI*2);ctx.strokeStyle='#ffe27a';ctx.lineWidth=3;ctx.stroke();};
    if(sel.kind==='planet'){const p=b.planets?.[sel.index];if(p)ring(p.x,p.y,p.radius+5);}
    else if(sel.kind==='actual'){const p=snapshot.actual_planets?.[sel.index];if(p)ring(p.x,p.y,p.radius+5);}
    else if(sel.kind==='hypothesis'){const h=b.field_hypothesis;if(h)ring(h.x,h.y,14);}
    else if(sel.kind==='player'){const p=snapshot.world?.players?.[sel.id];if(p)ring(p.x,p.y,13);}
    else if(sel.kind==='contact'){const p=b.contact_points?.[sel.index];if(p)ring(p.x,p.y,8);}
    else if(sel.kind==='spawn'){const p=snapshot.diagnostic?.bot_state?.observed_spawn_locations?.[sel.index];if(p)ring(p.x,p.y,13);}
    else if(sel.kind==='point')ring(sel.x,sel.y,5);
  }
  const matchNumbers=m=>`center ${fmt(m.distance,1)} off · radius ${sgn(m.radiusError)} · mass ×${fmt(m.massRatio,2)}`;
  function drawHypothesis(){
    const h=snapshot?.diagnostic?.map_belief?.field_hypothesis;if(!h)return;
    const r=9;ctx.save();ctx.strokeStyle=ctx.fillStyle='#ff7bd5';ctx.lineWidth=2.5;ctx.beginPath();ctx.moveTo(h.x,h.y-r);ctx.lineTo(h.x+r,h.y);ctx.lineTo(h.x,h.y+r);ctx.lineTo(h.x-r,h.y);ctx.closePath();ctx.stroke();ctx.font='12px system-ui';ctx.fillText('field-only source',h.x+r+3,h.y+4);ctx.restore();
  }
  function mapAgeRows(){const d=snapshot?.diagnostic||{},rows=[];if(d.map_pending_events!=null)rows.push(['Map events pending',String(d.map_pending_events)]);if(d.map_wait_ms!=null)rows.push(['Waited for map',`${d.map_wait_ms} ms${d.map_wait_fallback?' (gave up waiting)':''}`]);return rows;}
  function renderDisclosedList(){
    const el=$('disclosedList');el.replaceChildren();
    const actual=snapshot?.actual_planets;if(!actual)return;
    if(!snapshot.planet_match){el.textContent='No belief from this map epoch to compare with at this point.';return;}
    const statuses=disclosedStatuses(),rank={missing:0,both:1,location:2,size:3,ok:4};
    const order=actual.map((p,i)=>i).sort((a,b)=>rank[statuses[a].status]-rank[statuses[b].status]||(statuses[b].match?.distance??0)-(statuses[a].match?.distance??0)||a-b);
    const count=s=>statuses.filter(x=>x.status===s).length;
    const summary=document.createElement('div');summary.className='note';summary.textContent=`${count('missing')} missing · ${count('both')+count('location')+count('size')} off · ${count('ok')} close · ${snapshot.planet_match?.unmatched_estimated?.length??0} estimates match nothing`;
    const table=document.createElement('table');table.className='table';
    const row=(cells,cls)=>{const tr=document.createElement('tr');if(cls)tr.className=cls;for(const c of cells){const td=document.createElement(cls==='head'?'th':'td');td.textContent=c;tr.append(td);}return tr;};
    table.append(row(['#','center','r','status','Δ center','Δ r'],'head'));
    for(const i of order){
      const p=actual[i],s=statuses[i],tr=row([String(i),`${fmt(p.x,0)}, ${fmt(p.y,0)}`,fmt(p.radius,1),({ok:'close',location:'center',size:'size',both:'both',missing:'missing'})[s.status],s.match?fmt(s.match.distance,1):'—',s.match?sgn(s.match.radiusError):'—'],`row${sel?.kind==='actual'&&sel.index===i?' pinned':''}`);
      tr.children[3].style.color=STATUS_COLORS[s.status];
      tr.addEventListener('click',()=>{sel={kind:'actual',index:i};render();});
      table.append(tr);
    }
    el.append(summary,table);
  }
  function renderPlanetList(){
    const el=$('planetList'),b=snapshot?.diagnostic?.map_belief;el.replaceChildren();
    if(!b){el.textContent='No belief snapshot at this point.';return;}
    const planets=b.planets||[];if(!planets.length){el.textContent='No planets estimated yet.';return;}
    const actual=snapshot.actual_planets,matches=planetMatches();
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
  loadOverview();
  fillCaptureSelect().catch(e=>setStatus(e.message));
})();
