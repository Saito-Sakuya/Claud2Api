"use strict";
window.VersionManager = (() => {
  const $=id=>document.getElementById(id);
  let state, selected, loadedVersion, interval, poll, generation=0, pending, reloading=false, activeJob, activeSince, policyPending=false, policyValue, applyPending=false;
  const phases={queued:"准备更新",downloading:"正在下载版本",verifying:"正在校验版本",waiting:"等待当前请求结束",switching:"正在切换版本",backing_up:"正在保护现有数据",starting:"正在启动并验证",recovering:"正在恢复原版本",completed:"版本切换完成",failed:"版本切换未完成",idle:""};
  function setVersion(version) {
    loadedVersion ||= version;
    $("version-number").textContent=version;
    $("version-entry").setAttribute("aria-label",`版本管理，当前 ${version}${state?.update_available || state?.patch_update ? "，有新版本可用" : ""}`);
    $("version-current").textContent="当前版本 · " + version;
  }
  function render() {
    if (!state) return;
    setVersion(state.current_version);
    $("version-entry").classList.toggle("has-update",Boolean(state.update_available || state.patch_update));
    $("version-entry").classList.toggle("version-busy",Boolean(state.busy));
    const messages={"repository has not published releases yet":"仓库尚未发布可更新版本","GitHub is temporarily unavailable":"暂时无法连接 GitHub，将自动重试","GitHub rate limit reached; retry deferred":"GitHub 请求额度不足，已延后重试"};
    $("version-check-status").textContent=state.checking ? "正在向 GitHub 检查版本…" : !state.enabled ? "当前运行方式未启用自动更新" : state.check_error ? (messages[state.check_error] || state.check_error) : state.update_available || state.patch_update ? "发现新版本" : "已是最新可用版本";
    renderAutoPatch();
    const options=$("version-options");options.replaceChildren();
    const patch=state.patch_update?.version ? state.patch_update : undefined;
    const releases=[];
    if (state.latest?.version) releases.push({...state.latest,kind:patch?.version===state.latest.version ? "patch" : "feature"});
    if (patch && !releases.some(r=>r.version===patch.version)) releases.push({...patch,kind:"patch"});
    for (const rollback of state.rollback_versions || []) if (!releases.some(r=>r.version===rollback.version)) releases.push({...rollback,kind:"rollback"});
    if (!releases.some(r=>r.version===selected)) selected=undefined;
    const blocked=new Set(state.auto_patch?.blocked_versions || []);
    for (const release of releases) {
      const label=document.createElement("label"),input=document.createElement("input"),text=document.createElement("span"),name=document.createElement("strong"),hint=document.createElement("small");
      label.className="version-choice";input.type="radio";input.name="release-version";input.value=release.version;input.checked=selected===release.version;input.disabled=Boolean(state.busy || policyPending);
      name.textContent=release.version;
      hint.textContent=release.kind==="rollback" ? "回退到此版本" : release.kind==="patch" ? (blocked.has(release.version) ? "之前安装未完成，已暂停自动重试；可手动更新" : "补丁更新") : "功能版本更新，需手动确认";
      text.append(name,hint);label.append(input,text);options.append(label);
      input.addEventListener("change",()=>{selected=release.version;renderAction();});
    }
    if (!state.rollback_versions?.length) {const empty=document.createElement("p");empty.className="muted";empty.textContent="暂无可回退的发行版本";options.append(empty);}
    if (state.history_truncated) {const note=document.createElement("p");note.className="muted";note.textContent="更早的发行历史未全部加载";options.append(note);}
    const job=state.job || {};
    $("version-job").hidden=!job.id;
    $("version-job-label").textContent=(job.automatic ? "自动安装 · " : "") + (phases[job.phase] || job.phase || "") + (job.recovered ? " · 已恢复原版本" : "");
    $("version-job-detail").textContent=job.error || (job.target ? "目标版本 · " + job.target : "");
    if (job.total && job.phase==="downloading") {$("version-progress").value=state.downloaded/job.total;$("version-progress").max=1;}
    else if (state.busy) $("version-progress").removeAttribute("value");
    else $("version-progress").value=job.phase==="completed"?1:0;
    $("version-check").disabled=!window.Claud2ApiUI?.loggedIn() || Boolean(state.checking || state.busy || policyPending || applyPending);
    renderAction();
  }
  function supportsAutoPatch() {return Boolean(state?.enabled && state.auto_patch_supported===true && state.auto_patch);}
  function renderAutoPatch() {
    const section=$("version-auto-patch"),toggle=$("version-auto-patch-toggle"),input=$("version-auto-patch-input"),unsupported=$("version-auto-patch-unsupported");
    section.hidden=!state.enabled;
    const supported=supportsAutoPatch();
    toggle.hidden=!supported;
    unsupported.hidden=supported || !state.enabled;
    if (supported) input.checked=policyPending ? Boolean(policyValue) : Boolean(state.auto_patch.enabled);
    input.disabled=!supported || Boolean(state.busy || policyPending || applyPending);
  }
  function renderAction() {
    const draft=window.Claud2ApiUI?.hasDraft();
    $("version-draft-note").hidden=!draft;
    const rollback=state?.rollback_versions?.some(r=>r.version===selected);
    $("version-apply").textContent=selected ? (rollback ? "回退到 " : "更新到 ") + selected : "选择目标版本";
    $("version-apply").disabled=!selected || !state?.enabled || Boolean(state.busy || draft || policyPending || applyPending);
  }
  function schedule() {clearTimeout(interval);interval=setTimeout(()=>refresh(),30000);}
  async function refresh() {
    if (!window.Claud2ApiUI?.loggedIn() || pending) return null;
    const expected=generation,ticket={};pending=ticket;
    try {
      const value=await window.Claud2ApiUI.request("/updates");
      if (expected!==generation) return false;
      state=value;render();
      if (state.current_version!==loadedVersion && !state.busy && !reloading) {
        $("version-reload").hidden=false;
        if (!window.Claud2ApiUI.hasDraft()) { await reload(); if(expected!==generation) return false; }
      }
      clearTimeout(poll);
      if (activeJob && !state.busy && state.job?.id!==activeJob && Date.now()-activeSince>15000) {
        activeJob=undefined;$("version-check-status").textContent="服务已重新启动，请重新检查版本状态";
      }
      if (state.busy || state.checking || (activeJob && state.job?.id!==activeJob)) poll=setTimeout(()=>refresh(),1000);
      else {activeJob=undefined;schedule();}
      return true;
    } catch (error) {
      if (expected===generation) {
        $("version-check-status").textContent=error.message;
        if (activeJob || state?.busy) poll=setTimeout(()=>refresh(),2000); else schedule();
      }
      return false;
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
    if (!state || !selected || state.busy || policyPending || applyPending || window.Claud2ApiUI.hasDraft()) return;
    const expected=generation;let failure;
    applyPending=true;render();
    try {
      const result=await window.Claud2ApiUI.request("/updates/apply","POST",{expected_version:state.current_version,target_version:selected,catalog_revision:state.catalog_revision});
      if (expected!==generation) return;
      activeJob=result.job_id;activeSince=Date.now();
      const refreshed=await refresh();
      if (expected!==generation) return;
      if (refreshed===false) failure=$("version-check-status").textContent || undefined;
      if (!refreshed) {clearTimeout(poll);poll=setTimeout(()=>refresh(),1000);}
    } catch(error) {if(expected!==generation)return;failure=error.message;}
    finally {if(expected===generation){applyPending=false;render();if(failure)$("version-check-status").textContent=failure;}}
  });
  $("version-auto-patch-input").addEventListener("change",async event=>{
    if (!supportsAutoPatch() || state.busy || policyPending || applyPending) {renderAutoPatch();return;}
    const expected=generation;
    const revision=state.auto_patch.revision;
    if (!Number.isSafeInteger(revision) || revision<=0) {
      $("version-check-status").textContent="自动补丁设置版本无效，请刷新后重试";
      renderAutoPatch();return;
    }
    policyPending=true;policyValue=event.currentTarget.checked;const desired=policyValue;let saved=false,refreshPending=false,failure;render();
    try {
      await window.Claud2ApiUI.request("/updates/policy","POST",{expected_revision:revision,auto_patch:desired});
      if (expected!==generation) return;
      saved=true;
      const refreshed=await refresh();
      if (expected!==generation) return;
      if (refreshed===false) {
        const message=$("version-check-status").textContent;
        if (message) failure=message; else refreshPending=true;
      } else if (refreshed===null) refreshPending=true;
    } catch(error) {
      if(expected!==generation)return;
      failure=error.message;
    } finally {
      if (expected===generation) {
        policyPending=false;policyValue=undefined;render();
        if (failure) $("version-check-status").textContent=failure;
        if (saved && (refreshPending || (supportsAutoPatch() && Boolean(state.auto_patch.enabled)!==desired))) {clearTimeout(poll);poll=setTimeout(()=>refresh(),500);}
      }
    }
  });
  $("version-reload").addEventListener("click",reload);
  function disconnected() {generation++;pending=undefined;policyPending=false;policyValue=undefined;applyPending=false;clearTimeout(interval);clearTimeout(poll);state=undefined;selected=undefined;activeJob=undefined;$("version-entry").classList.remove("has-update","version-busy");$("version-options").replaceChildren();$("version-auto-patch").hidden=true;$("version-auto-patch-input").checked=false;$("version-auto-patch-input").disabled=true;$("version-apply").disabled=true;$("version-job").hidden=true;if ($("version-dialog").open) $("version-dialog").close();}
  function connected() {clearTimeout(interval);refresh();}
  function dispose() {generation++;clearTimeout(interval);clearTimeout(poll);}
  return {setVersion,connected,disconnected,dispose};
})();
