"use strict";
window.VersionManager = (() => {
  const $=id=>document.getElementById(id);
  let state, selected, loadedVersion, interval, poll, generation=0, pending, reloading=false, activeJob, activeSince;
  const phases={queued:"准备更新",downloading:"正在下载版本",verifying:"正在校验版本",waiting:"等待当前请求结束",switching:"正在切换版本",backing_up:"正在保护现有数据",starting:"正在启动并验证",recovering:"正在恢复原版本",completed:"版本切换完成",failed:"版本切换未完成",idle:""};
  function setVersion(version) {
    loadedVersion ||= version;
    $("version-number").textContent=version;
    $("version-entry").setAttribute("aria-label",`版本管理，当前 ${version}${state?.update_available ? "，有新版本可用" : ""}`);
    $("version-current").textContent="当前版本 · " + version;
  }
  function render() {
    if (!state) return;
    setVersion(state.current_version);
    $("version-entry").classList.toggle("has-update",Boolean(state.update_available));
    $("version-entry").classList.toggle("version-busy",Boolean(state.busy));
    const messages={"repository has not published releases yet":"仓库尚未发布可更新版本","GitHub is temporarily unavailable":"暂时无法连接 GitHub，将自动重试","GitHub rate limit reached; retry deferred":"GitHub 请求额度不足，已延后重试"};
    $("version-check-status").textContent=state.checking ? "正在向 GitHub 检查版本…" : !state.enabled ? "当前运行方式未启用自动更新" : state.check_error ? (messages[state.check_error] || state.check_error) : state.update_available ? "发现新版本" : "已是最新可用版本";
    const options=$("version-options");options.replaceChildren();
    const releases=[...(state.latest ? [{...state.latest,kind:"update"}] : []),...(state.rollback_versions || []).map(r=>({...r,kind:"rollback"}))];
    if (!releases.some(r=>r.version===selected)) selected=undefined;
    for (const release of releases) {
      const label=document.createElement("label"),input=document.createElement("input"),text=document.createElement("span"),name=document.createElement("strong"),hint=document.createElement("small");
      label.className="version-choice";input.type="radio";input.name="release-version";input.value=release.version;input.checked=selected===release.version;input.disabled=Boolean(state.busy);
      name.textContent=release.version;hint.textContent=release.kind==="update"?"更新到此版本":"回退到此版本";
      text.append(name,hint);label.append(input,text);options.append(label);
      input.addEventListener("change",()=>{selected=release.version;renderAction();});
    }
    if (!state.rollback_versions?.length) {const empty=document.createElement("p");empty.className="muted";empty.textContent="暂无可回退的发行版本";options.append(empty);}
    if (state.history_truncated) {const note=document.createElement("p");note.className="muted";note.textContent="更早的发行历史未全部加载";options.append(note);}
    const job=state.job || {};
    $("version-job").hidden=!job.id;
    $("version-job-label").textContent=(phases[job.phase] || job.phase || "") + (job.recovered ? " · 已恢复原版本" : "");
    $("version-job-detail").textContent=job.error || (job.target ? "目标版本 · " + job.target : "");
    if (job.total && job.phase==="downloading") {$("version-progress").value=state.downloaded/job.total;$("version-progress").max=1;}
    else if (state.busy) $("version-progress").removeAttribute("value");
    else $("version-progress").value=job.phase==="completed"?1:0;
    $("version-check").disabled=!window.Claud2ApiUI?.loggedIn() || Boolean(state.checking || state.busy);
    renderAction();
  }
  function renderAction() {
    const draft=window.Claud2ApiUI?.hasDraft();
    $("version-draft-note").hidden=!draft;
    const rollback=state?.rollback_versions?.some(r=>r.version===selected);
    $("version-apply").textContent=selected ? (rollback ? "回退到 " : "更新到 ") + selected : "选择目标版本";
    $("version-apply").disabled=!selected || !state?.enabled || Boolean(state.busy || draft);
  }
  function schedule() {clearTimeout(interval);interval=setTimeout(()=>refresh(),300000);}
  async function refresh() {
    if (!window.Claud2ApiUI?.loggedIn() || pending) return;
    const expected=generation,ticket={};pending=ticket;
    try {
      const value=await window.Claud2ApiUI.request("/updates");
      if (expected!==generation) return;
      state=value;render();
      if (state.current_version!==loadedVersion && !state.busy && !reloading) {
        $("version-reload").hidden=false;
        if (!window.Claud2ApiUI.hasDraft()) { await reload(); if(expected!==generation) return; }
      }
      clearTimeout(poll);
      if (activeJob && !state.busy && state.job?.id!==activeJob && Date.now()-activeSince>15000) {
        activeJob=undefined;$("version-check-status").textContent="服务已重新启动，请重新检查版本状态";
      }
      if (state.busy || state.checking || (activeJob && state.job?.id!==activeJob)) poll=setTimeout(()=>refresh(),1000);
      else {activeJob=undefined;schedule();}
    } catch (error) {
      if (expected===generation) {
        $("version-check-status").textContent=error.message;
        if (activeJob || state?.busy) poll=setTimeout(()=>refresh(),2000); else schedule();
      }
    } finally {if(pending===ticket) pending=undefined;}
  }
  async function reload() {
    if (reloading) return;reloading=true;
    try {await window.Claud2ApiUI.reload();}
    catch(error) {reloading=false;$("version-check-status").textContent=error.message;$("version-reload").hidden=false;}
  }
  $("version-entry").addEventListener("click",()=>{
    if (!$("version-dialog").open) $("version-dialog").showModal();
    if (window.Claud2ApiUI?.loggedIn()) {renderAction();refresh();}
    else $("version-check-status").textContent="请先连接后端，再检查更新与历史版本";
  });
  $("version-close").addEventListener("click",()=>$("version-dialog").close());
  $("version-check").addEventListener("click",async()=>{
    try {await window.Claud2ApiUI.request("/updates/check","POST",{});await refresh();clearTimeout(poll);poll=setTimeout(()=>refresh(),1000);}
    catch(error) {$("version-check-status").textContent=error.message;}
  });
  $("version-apply").addEventListener("click",async()=>{
    if (!state || !selected || state.busy || window.Claud2ApiUI.hasDraft()) return;
    $("version-apply").disabled=true;
    try {
      const result=await window.Claud2ApiUI.request("/updates/apply","POST",{expected_version:state.current_version,target_version:selected,catalog_revision:state.catalog_revision});
      activeJob=result.job_id;activeSince=Date.now();await refresh();
    } catch(error) {$("version-check-status").textContent=error.message;renderAction();}
  });
  $("version-reload").addEventListener("click",reload);
  function disconnected() {generation++;pending=undefined;clearTimeout(interval);clearTimeout(poll);state=undefined;selected=undefined;activeJob=undefined;$("version-entry").classList.remove("has-update","version-busy");$("version-options").replaceChildren();$("version-apply").disabled=true;$("version-job").hidden=true;if ($("version-dialog").open) $("version-dialog").close();}
  function connected() {clearTimeout(interval);refresh();}
  function dispose() {generation++;clearTimeout(interval);clearTimeout(poll);}
  return {setVersion,connected,disconnected,dispose};
})();
