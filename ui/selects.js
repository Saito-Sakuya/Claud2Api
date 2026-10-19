"use strict";
window.SelectUI = (() => {
  const lifecycle=new AbortController();
  let disposed=false;
  const controls = new Map();
  let openControl;
  function close() {
    if (!openControl) return;
    openControl.menu.hidden = true;
    openControl.button.setAttribute("aria-expanded", "false");
    openControl.button.removeAttribute("aria-activedescendant");
    openControl = undefined;
  }
  function enhance(select) {
    if (controls.has(select) || select.multiple || select.size > 1) return;
    const shell = document.createElement("div"),
      button = document.createElement("button"),
      value = document.createElement("span"),
      menu = document.createElement("div");
    shell.className = "select-shell";
    button.type = "button";
    button.className = "select-trigger";
    button.id = select.id + "-trigger";
    button.setAttribute("role", "combobox");
    button.setAttribute("aria-haspopup", "listbox");
    button.setAttribute("aria-expanded", "false");
    menu.id = select.id + "-listbox";
    menu.className = "select-popup";
    menu.setAttribute("role", "listbox");
    menu.hidden = true;
    button.setAttribute("aria-controls", menu.id);
    button.append(value);
    select.before(shell);
    shell.append(select, button);
    document.body.append(menu);
    // Keep the native element as the form/state source, but never open its OS popup.
    select.classList.add("select-source");
    select.tabIndex = -1;
    select.setAttribute("aria-hidden", "true");
    const control = {
      select,
      shell,
      button,
      value,
      menu,
      active: -1,
      options: [],
      signature: "",
      prefix: "",
      typedAt: 0,
    };
    controls.set(select, control);
    const labels = [...select.labels];
    if (select.getAttribute("aria-label")) {
      button.setAttribute("aria-label", select.getAttribute("aria-label"));
      menu.setAttribute("aria-label", select.getAttribute("aria-label"));
    } else if (labels.length) {
      labels.forEach((label, i) => {
        if (!label.id) label.id = select.id + "-label-" + i;
        label.addEventListener("click", (event) => {
          event.preventDefault();
          button.focus();
        });
      });
      const ids = labels.map((label) => label.id).join(" ");
      button.setAttribute("aria-labelledby", ids);
      menu.setAttribute("aria-labelledby", ids);
    }
    function activate(index) {
      control.active = index;
      [...menu.children].forEach((item, i) =>
        item.classList.toggle("active", i === index)
      );
      const active = menu.children[index];
      if (active) {
        button.setAttribute("aria-activedescendant", active.id);
        active.scrollIntoView({ block: "nearest" });
      }
    }
    function choose(index) {
      const option = control.options[index];
      if (!option || option.disabled) return;
      select.value = option.value;
      close();
      sync(control);
      select.dispatchEvent(new Event("input", { bubbles: true }));
      select.dispatchEvent(new Event("change", { bubbles: true }));
      button.focus();
    }
    function open() {
      if (button.disabled) return;
      close();
      sync(control);
      if (!control.options.some((o) => !o.disabled)) return;
      openControl = control;
      menu.hidden = false;
      button.setAttribute("aria-expanded", "true");
      position(control);
      const selected = select.selectedIndex;
      activate(
        selected >= 0 && !control.options[selected].disabled
          ? selected
          : control.options.findIndex((o) => !o.disabled)
      );
    }
    button.addEventListener("click", () =>
      openControl === control ? close() : open()
    );
    button.addEventListener("keydown", (event) => {
      const isOpen = openControl === control;
      if (event.key === "Escape") {
        if (isOpen) {
          event.preventDefault();
          close();
        }
        return;
      }
      if (event.key === "Tab") {
        close();
        return;
      }
      if (
        ["ArrowDown", "ArrowUp", "Home", "End", "Enter", " "].includes(
          event.key
        )
      ) {
        event.preventDefault();
        if (!isOpen) {
          open();
          return;
        }
        if (event.key === "Enter" || event.key === " ") {
          choose(control.active);
          return;
        }
        const enabled = control.options
          .map((o, i) => (o.disabled ? -1 : i))
          .filter((i) => i >= 0);
        const current = enabled.indexOf(control.active);
        const next =
          event.key === "Home"
            ? 0
            : event.key === "End"
            ? enabled.length - 1
            : Math.max(
                0,
                Math.min(
                  enabled.length - 1,
                  current + (event.key === "ArrowUp" ? -1 : 1)
                )
              );
        activate(enabled[next]);
      } else if (
        event.key.length === 1 &&
        !event.ctrlKey &&
        !event.metaKey &&
        !event.altKey
      ) {
        event.preventDefault();
        if (!isOpen) open();
        const now = Date.now();
        control.prefix =
          (now - control.typedAt > 700 ? "" : control.prefix) +
          event.key.toLocaleLowerCase();
        control.typedAt = now;
        const next = control.options.findIndex(
          (o) =>
            !o.disabled && o.text.toLocaleLowerCase().startsWith(control.prefix)
        );
        if (next >= 0) activate(next);
      }
    });
    button.addEventListener("blur", close);
    menu.addEventListener("pointerdown", (event) => event.preventDefault());
    menu.addEventListener("pointermove", (event) => {
      const item = event.target.closest('[role="option"]');
      if (item && item.getAttribute("aria-disabled") !== "true")
        activate(Number(item.dataset.index));
    });
    menu.addEventListener("click", (event) => {
      const item = event.target.closest('[role="option"]');
      if (item) choose(Number(item.dataset.index));
    });
    select.addEventListener("change", () => {
      if (select.validity.valid) button.removeAttribute("aria-invalid");
      sync(control);
    });
    select.addEventListener("invalid", (event) => {
      event.preventDefault();
      button.setAttribute("aria-invalid", "true");
      button.focus();
      open();
    });
    control.observer = new MutationObserver(() => sync(control));
    control.observer.observe(select, {
      subtree: true,
      childList: true,
      attributes: true,
      characterData: true,
    });
    select.form?.addEventListener("reset", () =>
      queueMicrotask(() => sync(control))
    );
    sync(control);
  }
  function sync(control) {
    const { select, button, value, menu } = control;
    const options = [...select.options].map((o) => ({
      value: o.value,
      text: o.textContent,
      disabled: o.disabled || o.parentElement?.disabled,
    }));
    const signature = JSON.stringify([options, select.selectedIndex]);
    button.disabled = select.disabled || !options.some((o) => !o.disabled);
    button.setAttribute("aria-required", String(select.required));
    value.textContent = select.selectedOptions[0]?.textContent || "请选择";
    if (control.signature === signature) return;
    control.signature = signature;
    control.options = options;
    menu.replaceChildren(
      ...options.map((o, i) => {
        const item = document.createElement("div");
        item.id = select.id + "-option-" + i;
        item.className = "select-option";
        item.setAttribute("role", "option");
        item.setAttribute("aria-selected", String(i === select.selectedIndex));
        item.setAttribute("aria-disabled", String(Boolean(o.disabled)));
        item.dataset.index = i;
        item.textContent = o.text;
        return item;
      })
    );
    if (openControl === control) {
      if (button.disabled || !button.getClientRects().length) close();
      else {
        control.active = select.selectedIndex;
        button.setAttribute(
          "aria-activedescendant",
          select.id + "-option-" + select.selectedIndex
        );
        position(control);
      }
    }
  }
  function position(control) {
    const rect = control.button.getBoundingClientRect(),
      menu = control.menu;
    const below = innerHeight - rect.bottom - 12,
      above = rect.top - 12,
      up = below < 180 && above > below;
    menu.style.width =
      Math.min(Math.max(rect.width, 150), innerWidth - 20) + "px";
    menu.style.maxHeight =
      Math.max(60, Math.min(280, up ? above : below)) + "px";
    menu.style.left =
      Math.max(
        10,
        Math.min(
          rect.left,
          innerWidth - menu.getBoundingClientRect().width - 10
        )
      ) + "px";
    menu.style.top =
      (up
        ? Math.max(10, rect.top - menu.getBoundingClientRect().height - 6)
        : rect.bottom + 6) + "px";
  }
  document.addEventListener("pointerdown", (event) => {
    if (
      openControl &&
      !openControl.shell.contains(event.target) &&
      !openControl.menu.contains(event.target)
    )
      close();
  }, {signal:lifecycle.signal});
  document.addEventListener(
    "scroll",
    (event) => {
      if (openControl && !openControl.menu.contains(event.target)) close();
    },
    {capture:true,signal:lifecycle.signal}
  );
  window.addEventListener("resize", () => {
    if (openControl) position(openControl);
  }, {signal:lifecycle.signal});
  window.addEventListener("pagehide", close, {signal:lifecycle.signal});
  const syncAll = () => {
    if (disposed) return;
    for (const [select, control] of controls) {
      if (!select.isConnected) {
        if (openControl === control) close();
        control.observer.disconnect();
        control.menu.remove();
        controls.delete(select);
      }
    }
    document.querySelectorAll("select").forEach(enhance);
    controls.forEach(sync);
  };
  document.addEventListener("click", () => queueMicrotask(syncAll), {signal:lifecycle.signal});
  syncAll();
  return { sync: syncAll, close, dispose() { disposed=true;close();lifecycle.abort();for(const c of controls.values()){c.observer.disconnect();c.menu.remove();}controls.clear();} };
})();
