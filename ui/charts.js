"use strict";
window.UsageCharts = (() => {
  const $ = (id) => document.getElementById(id);
  const table = (...args) => window.Claud2ApiUI.table(...args);
  const colors = [
    "#88d8bc",
    "#a3b5f1",
    "#e4b67a",
    "#dc9bb8",
    "#82c6d9",
    "#c3bb86",
    "#bba1e1",
    "#98bb8b",
    "#d89b8c",
    "#8ca8c9",
    "#d2a6d7",
    "#9cc9b6",
    "#87918e",
  ];
  const hidden = new Set();
  let current;
  const num = window.UsageFormat.number;
  const compact = num;
  const money = window.UsageFormat.money;
  const cost = (t) =>
    t.priced_requests || !t.records ? money(t.total_nano_usd) : "未计价";
  const tokens = (t) => t.input_estimate + t.output_estimate;
  function node(tag, text, cls) {
    const n = document.createElement(tag);
    if (text !== undefined) n.textContent = text;
    if (cls) n.className = cls;
    return n;
  }
  function svg(tag, attrs = {}, text) {
    const n = document.createElementNS("http://www.w3.org/2000/svg", tag);
    for (const [k, v] of Object.entries(attrs)) n.setAttribute(k, String(v));
    if (text !== undefined) n.textContent = text;
    return n;
  }
  function stamp(at, zone, short = false) {
    return new Intl.DateTimeFormat("zh-CN", {
      timeZone: zone,
      month: "2-digit",
      day: "2-digit",
      ...(short
        ? { hour: "2-digit", minute: "2-digit" }
        : { year: "numeric", hour: "2-digit", minute: "2-digit" }),
    }).format(at * 1000);
  }
  function metric(t) {
    switch ($("usage-metric").value) {
      case "cost":
        return t.unpriced_requests || t.unmetered_requests
          ? null
          : t.total_nano_usd / 1e9;
      case "input":
        return t.input_estimate;
      case "output":
        return t.output_estimate;
      default:
        return tokens(t);
    }
  }
  function trend(data) {
    const host = $("usage-trend"),
      legend = $("usage-legend");
    host.replaceChildren();
    legend.replaceChildren();
    const series = data.series,
      window = data.trend_window;
    if (
      !series.length ||
      !series.some((s) => s.points.some((p) => p.records))
    ) {
      host.append(node("p", "当前趋势窗口暂无记录", "empty"));
      return;
    }
    const width = 1000,
      height = 295,
      left = 70,
      right = 25,
      top = 24,
      bottom = 43;
    const active = series.filter((s) => !hidden.has(s.id));
    const observed = Math.max(
      0,
      ...active.flatMap((s) => s.points.map((p) => metric(p) || 0))
    );
    const max =
      (observed || ($("usage-metric").value === "cost" ? 0.001 : 1)) * 1.08;
    const count = window.buckets.length - 1;
    const x = (i) =>
        left + ((width - left - right) * i) / Math.max(1, count - 1),
      y = (v) => height - bottom - ((height - top - bottom) * v) / max;
    const picture = svg("svg", {
      viewBox: `0 0 ${width} ${height}`,
      role: "img",
      "aria-label": `不同令牌的${
        data.granularity === "hour" ? "24小时" : "7天"
      }用量曲线`,
    });
    picture.append(
      svg("title", {}, "令牌用量趋势；悬停数据点查看数值，明细见下方表格")
    );
    for (let i = 0; i <= 4; i++) {
      const value = (max * i) / 4,
        yy = y(value);
      picture.append(
        svg("line", {
          x1: left,
          x2: width - right,
          y1: yy,
          y2: yy,
          class: "chart-grid",
        })
      );
      picture.append(
        svg(
          "text",
          {
            x: left - 12,
            y: yy + 4,
            "text-anchor": "end",
            class: "chart-axis",
          },
          $("usage-metric").value === "cost"
            ? money(value * 1e9)
            : compact(value)
        )
      );
    }
    for (let i = 0; i < count; i++) {
      if (i % Math.ceil(count / 6) === 0 || i === count - 1)
        picture.append(
          svg(
            "text",
            {
              x: x(i),
              y: height - 12,
              "text-anchor": "middle",
              class: "chart-axis",
            },
            new Intl.DateTimeFormat("zh-CN", {
              timeZone: data.timezone,
              ...(data.granularity === "hour"
                ? { hour: "2-digit", minute: "2-digit" }
                : { month: "2-digit", day: "2-digit" }),
            }).format(window.buckets[i] * 1000)
          )
        );
    }
    series.forEach((s, index) => {
      const color = colors[index % colors.length],
        selected = !hidden.has(s.id);
      const toggle = node(
        "button",
        undefined,
        "legend-toggle" + (selected ? "" : " inactive")
      );
      toggle.type = "button";
      toggle.setAttribute("aria-pressed", String(selected));
      const dot = svg("svg", { width: 12, height: 12, "aria-hidden": true });
      dot.append(svg("circle", { cx: 6, cy: 6, r: 4, fill: color }));
      toggle.append(dot, node("span", s.name));
      toggle.addEventListener("click", () => {
        selected ? hidden.add(s.id) : hidden.delete(s.id);
        trend(data);
      });
      legend.append(toggle);
      if (!selected) return;
      let path = "",
        pen = false;
      s.points.forEach((p, i) => {
        const v = metric(p);
        if (v === null) {
          pen = false;
          return;
        }
        path += (pen ? "L" : "M") + x(i) + " " + y(v) + " ";
        pen = true;
      });
      picture.append(
        svg("path", {
          d: path,
          stroke: color,
          "stroke-width": 2,
          fill: "none",
          "stroke-linejoin": "round",
        })
      );
      s.points.forEach((p, i) => {
        const v = metric(p);
        if (v === null) return;
        const circle = svg("circle", {
          cx: x(i),
          cy: y(v),
          r: 3,
          fill: color,
          tabindex: 0,
          class: "chart-point",
        });
        circle.append(
          svg(
            "title",
            {},
            `${s.name}\n${stamp(
              window.buckets[i],
              data.timezone,
              true
            )} — ${stamp(window.buckets[i + 1], data.timezone, true)}\n${
              $("usage-metric").value === "cost"
                ? money(p.total_nano_usd)
                : num(v)
            }\n请求 ${p.requests} · 内部压缩 ${p.internal_requests}`
          )
        );
        picture.append(circle);
      });
    });
    host.append(picture);
    if (
      $("usage-metric").value === "cost" &&
      series.some((s) =>
        s.points.some((p) => p.unpriced_requests || p.unmetered_requests)
      )
    )
      host.append(
        node(
          "small",
          "缺少单价或计量的时间点留空，金额曲线不把未知费用当作零。"
        )
      );
    host.append(
      node(
        "small",
        `${stamp(window.start, data.timezone)} — ${stamp(
          window.end - 1,
          data.timezone
        )} · ${data.timezone}`
      )
    );
  }
  function donut(data) {
    const host = $("usage-donut");
    host.replaceChildren();
    const rows = [...data.tokens]
      .filter((r) => tokens(r.totals) > 0)
      .sort((a, b) => tokens(b.totals) - tokens(a.totals));
    const total = tokens(data.totals);
    if (!total) {
      host.append(node("p", "所选范围暂无用量", "empty"));
      return;
    }
    let shown = rows
      .slice(0, 8)
      .map((r) => ({ name: r.name, value: tokens(r.totals) }));
    if (rows.length > 8)
      shown.push({
        name: "其余令牌",
        value: rows.slice(8).reduce((s, r) => s + tokens(r.totals), 0),
      });
    const picture = svg("svg", {
      viewBox: "0 0 220 220",
      role: "img",
      "aria-label": "不同令牌的总Token用量占比",
    });
    picture.append(svg("title", {}, "使用者用量占比"));
    picture.append(
      svg("circle", {
        cx: 110,
        cy: 110,
        r: 80,
        fill: "none",
        stroke: "#23312c",
        "stroke-width": 24,
      })
    );
    let offset = 0;
    const circumference = 2 * Math.PI * 80,
      list = node("div", undefined, "donut-legend");
    shown.forEach((r, i) => {
      const fraction = r.value / total,
        color = colors[i % colors.length];
      const arc = svg("circle", {
        cx: 110,
        cy: 110,
        r: 80,
        fill: "none",
        stroke: color,
        "stroke-width": 24,
        "stroke-dasharray": `${fraction * circumference} ${circumference}`,
        "stroke-dashoffset": -offset * circumference,
        transform: "rotate(-90 110 110)",
      });
      arc.append(
        svg(
          "title",
          {},
          `${r.name}: ${num(r.value)} (${(fraction * 100).toFixed(2)}%)`
        )
      );
      picture.append(arc);
      offset += fraction;
      const item = node("div", undefined, "donut-row"),
        dot = svg("svg", { width: 12, height: 12, "aria-hidden": true });
      dot.append(svg("circle", { cx: 6, cy: 6, r: 4, fill: color }));
      item.append(
        dot,
        node("span", r.name),
        node(
          "strong",
          fraction < 0.001 ? "<0.1%" : (fraction * 100).toFixed(1) + "%"
        )
      );
      list.append(item);
    });
    picture.append(
      svg(
        "text",
        { x: 110, y: 108, "text-anchor": "middle", class: "donut-value" },
        compact(total)
      ),
      svg(
        "text",
        { x: 110, y: 131, "text-anchor": "middle", class: "chart-axis" },
        "总 Token"
      )
    );
    host.append(picture, list);
  }
  function render(data, info, names, settings) {
    current = { data, info, names };
    const t = data.totals;
    $("usage-window").textContent = `${stamp(
      data.window.start,
      data.timezone
    )} — ${stamp(data.window.end - 1, data.timezone)} · ${data.timezone}`;
    const diff = t.baseline_nano_usd - t.total_nano_usd;
    const metrics = [
      [
        cost(t),
        "模拟总金额 · USD",
        `${t.priced_requests} 条已计价${
          t.unpriced_requests ? ` · ${t.unpriced_requests} 条未计价` : ""
        }`,
      ],
      [
        num(t.requests),
        "业务请求记录",
        `${num(t.internal_requests)} 次内部压缩 · ${num(
          t.unmetered_requests
        )} 条消耗未知`,
      ],
      [
        num(t.input_estimate),
        "输入估算 Token",
        `普通 ${num(t.uncached_input)} · 创建 ${num(
          t.cache_creation_simulated
        )} · 读取 ${num(t.cache_read_simulated)}`,
      ],
      [
        num(t.output_estimate),
        "输出估算 Token",
        `${info.busy_accounts} / ${info.capacity} 执行槽 · ${info.sessions} 个会话`,
      ],
    ];
    $("metrics").replaceChildren(
      ...metrics.map(([value, label, note]) => {
        const n = node("div", undefined, "metric glass");
        n.append(
          node("span", value, "value"),
          node("span", label, "label"),
          node("small", note)
        );
        return n;
      })
    );
    trend(data);
    donut(data);
    table(
      "token-usage-table",
      [
        "令牌名称",
        "业务 / 压缩记录",
        "输入 / 输出估算",
        "1h 创建 / 读取",
        "模拟 USD",
      ],
      [...data.tokens]
        .sort((a, b) => tokens(b.totals) - tokens(a.totals))
        .map((r) => {
          const name = node("span", r.name);
          name.title = r.id;
          return [
            name,
            `${num(r.totals.requests)} / ${num(r.totals.internal_requests)}`,
            `${num(r.totals.input_estimate)} / ${num(
              r.totals.output_estimate
            )}`,
            `${num(r.totals.cache_creation_simulated)} / ${num(
              r.totals.cache_read_simulated
            )}`,
            cost(r.totals) +
              (r.totals.unpriced_requests ? " · 部分未计价" : ""),
          ];
        })
    );
    table(
      "usage-table",
      ["账号", "业务 / 压缩记录", "输入估算", "输出估算", "模拟 USD"],
      Object.entries(data.by_account).map(([id, r]) => [
        names.get(id) || id.slice(0, 8),
        `${num(r.requests)} / ${num(r.internal_requests)}`,
        num(r.input_estimate),
        num(r.output_estimate),
        cost(r),
      ])
    );
    table(
      "cost-breakdown",
      ["计费类别", "估算 Token", "模拟 USD"],
      [
        ["普通输入", num(t.priced_uncached_input), money(t.input_nano_usd)],
        [
          "1h 缓存创建",
          num(t.priced_cache_creation),
          money(t.cache_write_nano_usd),
        ],
        ["缓存读取", num(t.priced_cache_read), money(t.cache_read_nano_usd)],
        ["输出", num(t.priced_output), money(t.output_nano_usd)],
        ["无缓存基准（已计价记录）", "—", money(t.baseline_nano_usd)],
        [
          diff >= 0 ? "相对基准模拟减少" : "相对基准模拟增加",
          "—",
          money(Math.abs(diff)),
        ],
      ]
    );
    $("cost-note").textContent = `按 ${
      data.price_date
    } 官方 API 单价逐条计算，记录当时的缓存开关与费率。1h 写入按普通输入的 2 倍，读取按模型对应价格；首次创建可能增加金额。当前缓存展示${
      settings.simulated_cache ? "开启" : "关闭"
    }，历史金额保持原值。`;
    $(
      "usage-note"
    ).textContent = `金额为已计价记录小计，非实际账单；未知单价不按零价算。${num(t.unpriced_requests)} 条成功记录未计价，${num(t.legacy_records)} 条旧记录未补归属，${num(t.unmetered_requests)} 条失败消耗未知。请求记录不含尚未进入生成的拒绝，断连及外部服务用量可能缺失。运行以来覆盖持久账本，重启不清零。`;
  }
  return {
    render,
    redraw() {
      if (current) trend(current.data);
    },
    clear() {
      current = undefined;
      hidden.clear();
    },
  };
})();
