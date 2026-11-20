import { byId } from "./dom.ts";

interface Day {
  start: number;
  hours: number[];
}
interface Timing {
  count: number;
  mean: number | null;
  median: number | null;
  p90: number | null;
}
interface Activity {
  network: string;
  caught_up: boolean;
  checkpoint: { time: number; height: number };
  days: Day[];
  activation_day: number | null;
  weekdays: WeekdaySummary[];
  commitments: {
    from_calendar: number;
    from_coinbase: number;
    from_earlier: number;
    leaves: number;
    transactions: number;
    most_common: number[];
  };
  load: {
    blocks: number;
    mean_actions: number | null;
    capacity: number | null;
  };
  balance: { amount_zat: number; height: number; time: number } | null;
  pending: {
    transactions: number | null;
    observed_at: number | null;
    fresh: boolean;
  };
  ten_confirmations: Timing;
  first_confirmation: Timing;
}
interface WeekdaySummary {
  days: number;
  median: number | null;
  peak_hour: number | null;
}
interface DailyFlows {
  transactions: number;
  checked: number;
  ironwood_only: number;
  in_count: number;
  out_count: number;
  median_in_zat: number | null;
  median_out_zat: number | null;
}

const number = new Intl.NumberFormat("en-US", { maximumFractionDigits: 1 });
const amount = new Intl.NumberFormat("en-US", { maximumFractionDigits: 9 });
const date = (seconds: number) => new Date(seconds * 1000);
const shortDate = new Intl.DateTimeFormat("en-US", {
  timeZone: "UTC",
  month: "short",
  day: "numeric",
});
const longDate = new Intl.DateTimeFormat("en-US", {
  timeZone: "UTC",
  weekday: "short",
  month: "long",
  day: "numeric",
  year: "numeric",
});
const month = new Intl.DateTimeFormat("en-US", {
  timeZone: "UTC",
  month: "short",
});
const weekdayNames = [
  "Monday",
  "Tuesday",
  "Wednesday",
  "Thursday",
  "Friday",
  "Saturday",
  "Sunday",
];
const total = (d: Day) => d.hours.reduce((a, b) => a + b, 0);
const hour = (n: number) => String(n).padStart(2, "0");
const clock = (seconds: number) =>
  date(seconds).toISOString().slice(11, 16) + " UTC";
function write(id: string, value: string) {
  byId(id).textContent = value;
}
function duration(seconds: number | null) {
  if (seconds === null) return "—";
  const s = Math.round(seconds);
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${s % 60}s`;
}

let data: Activity | undefined;
let selected: number | undefined;
let requestPending = false;
let weekdayZone: string | undefined;
let weekdayRequest: AbortController | undefined;
const calendar = byId("calendar");
const calendarScroll = byId("calendar-scroll");
const daySeconds = 86400;
const yearOf = (start: number) => date(start).getUTCFullYear();
const yearStart = (year: number) => Date.UTC(year, 0, 1) / 1000;
const calendarDays = new Map<number, Day>();
const archivedYears = new Map<number, Day[]>();
const loadingYears = new Set<number>();
let calendarYear: number | undefined;
const poolPage = byId("pool-view");
const viewActive = () => !poolPage.hidden && !document.hidden;
const svgNS = "http://www.w3.org/2000/svg";

let flowDay: number | undefined;
let flowRequest: AbortController | undefined;
function renderFlows(flows?: DailyFlows) {
  const formatMedian = (zat?: number | null) =>
    zat == null ? "—" : amount.format(zat / 1e8);
  write("median-in", formatMedian(flows?.median_in_zat));
  write("median-out", formatMedian(flows?.median_out_zat));
  write(
    "count-in",
    flows ? `${number.format(flows.in_count)} inflows` : "Inflows",
  );
  write(
    "count-out",
    flows ? `${number.format(flows.out_count)} outflows` : "Outflows",
  );
}

async function showFlows(day?: Day) {
  if (flowDay !== day?.start) {
    flowRequest?.abort();
    flowRequest = undefined;
    flowDay = day?.start;
    write(
      "flow-date",
      day ? `${shortDate.format(date(day.start))} UTC · ZEC` : "ZEC",
    );
    renderFlows();
    write(
      "flow-window",
      day ? "Loading pool flows…" : "Waiting for indexed dates.",
    );
  }
  if (!day || flowRequest) return;
  const request = new AbortController();
  flowRequest = request;
  try {
    const response = await fetch(`/v1/activity/flows/${day.start}`, {
      cache: "no-store",
      signal: AbortSignal.any([request.signal, AbortSignal.timeout(12000)]),
    });
    if (!response.ok) throw new Error("Daily flows unavailable");
    const flows = (await response.json()) as DailyFlows;
    if (request.signal.aborted) return;
    renderFlows(flows);
    const coverage = `${number.format(flows.checked)} of ${number.format(flows.transactions)} transactions checked`;
    write(
      "flow-window",
      flows.checked < flows.transactions
        ? `Indexing · ${coverage}`
        : flows.transactions === 0
          ? "No confirmed transactions"
          : `${number.format(flows.ironwood_only)} Ironwood-only transactions excluded`,
    );
  } catch {
    if (!request.signal.aborted)
      write("flow-window", "Pool flows unavailable · Retrying");
  } finally {
    if (flowRequest === request) flowRequest = undefined;
  }
}

function showDay(day: Day) {
  selected = day.start;
  void showFlows(day);
  for (const button of calendar.querySelectorAll("button")) {
    const active = Number(button.dataset.start) === selected;
    button.setAttribute("aria-pressed", String(active));
    button.tabIndex = active ? 0 : -1;
  }
  write(
    "selected-date",
    shortDate.format(date(day.start)) + ", " + date(day.start).getUTCFullYear(),
  );
  write("day-total", number.format(total(day)));
  write(
    "hour-coverage",
    day.start === data?.days.at(-1)?.start ? "Through latest block" : "",
  );
  drawHours(day);
}

const hourTargets = byId("hour-targets");
let chartDay: Day | undefined;
let selectedHour: number | undefined;
const chartTime = (day: Day, h: number) => date(day.start + h * 3600);
const hourLabel = (day: Day, h: number) =>
  chartZone === "UTC" ? `${hour(h)}:00` : zoneClock.format(chartTime(day, h));
const hourRange = (day: Day, h: number) =>
  `${hourLabel(day, h)}–${hourLabel(day, h + 1)}`;
const hourTooltip = (day: Day, h: number) =>
  chartZone === "UTC"
    ? `${hourRange(day, h)} UTC`
    : `${zoneTimestamp.format(chartTime(day, h))} – ${zoneTimestamp.format(chartTime(day, h + 1))}`;
const transactionCount = (n: number) =>
  `${number.format(n)} ${n === 1 ? "transaction" : "transactions"}`;

function drawHours(day: Day) {
  const active = document.activeElement;
  const focusedHour =
    active instanceof HTMLButtonElement && hourTargets.contains(active)
      ? Number(active.dataset.hour)
      : undefined;
  if (chartDay?.start !== day.start) selectedHour = undefined;
  chartDay = day;
  for (const label of document.querySelectorAll<HTMLElement>(
    "[data-chart-hour]",
  )) {
    const h = Number(label.dataset.chartHour);
    label.textContent = chartZone === "UTC" ? hour(h) + "h" : hourLabel(day, h);
  }
  const chart = byId("hour-chart");
  chart.replaceChildren();
  hourTargets.replaceChildren();
  const peak = Math.max(...day.hours);
  const baseline = document.createElementNS(svgNS, "line");
  baseline.setAttribute("x2", "288");
  baseline.setAttribute("y1", "81");
  baseline.setAttribute("y2", "81");
  chart.append(baseline);
  day.hours.forEach((count, h) => {
    const target = document.createElement("button");
    target.type = "button";
    target.dataset.hour = String(h);
    target.title = `${hourTooltip(day, h)} · ${transactionCount(count)}`;
    target.setAttribute("aria-label", target.title);
    hourTargets.append(target);
    if (!count) return;
    const bar = document.createElementNS(svgNS, "rect");
    const height = Math.max(1, (count / peak) * 78);
    bar.setAttribute("x", String(h * 12 + 1));
    bar.setAttribute("width", "9");
    bar.setAttribute("y", String(81 - height));
    bar.setAttribute("height", String(height));
    bar.setAttribute("rx", "1.5");
    if (count === peak) bar.classList.add("peak-bar");
    chart.append(bar);
  });
  showHourDetail(day);
  if (focusedHour !== undefined) {
    hourTargets
      .querySelectorAll("button")
      [focusedHour]?.focus({ preventScroll: true });
  }
}

function showHourDetail(day: Day) {
  const peak = Math.max(...day.hours);
  const firstPeak = peak ? day.hours.indexOf(peak) : undefined;
  const activeHour = selectedHour ?? firstPeak;
  for (const button of hourTargets.querySelectorAll("button")) {
    const h = Number(button.dataset.hour);
    button.tabIndex = h === (activeHour ?? 0) ? 0 : -1;
    button.setAttribute("aria-pressed", String(h === selectedHour));
  }
  write(
    "hour-label",
    selectedHour === undefined ? "Peak hour" : "Selected hour",
  );
  write(
    "peak-hour",
    activeHour === undefined ? "—" : hourRange(day, activeHour),
  );
  byId("peak-hour").title =
    activeHour === undefined ? "" : hourTooltip(day, activeHour);
  const count = activeHour === undefined ? 0 : (day.hours[activeHour] ?? 0);
  write("peak-detail", transactionCount(count));
}

hourTargets.addEventListener("click", (event) => {
  if (!chartDay || !(event.target instanceof HTMLButtonElement)) return;
  selectedHour = Number(event.target.dataset.hour);
  showHourDetail(chartDay);
});
hourTargets.addEventListener("keydown", (event) => {
  if (!chartDay || !(event.target instanceof HTMLButtonElement)) return;
  const h = Number(event.target.dataset.hour);
  const last = chartDay.hours.length - 1;
  const destinations: Record<string, number> = {
    ArrowLeft: Math.max(0, h - 1),
    ArrowRight: Math.min(last, h + 1),
    Home: 0,
    End: last,
  };
  const next = destinations[event.key];
  if (next === undefined && event.key !== "Escape") return;
  event.preventDefault();
  selectedHour = next;
  showHourDetail(chartDay);
  hourTargets.querySelector<HTMLButtonElement>('button[tabindex="0"]')?.focus();
});

const zonePicker = byId("chart-timezone");
const zoneToggle = byId("timezone-toggle", "button");
const zonePopup = byId("timezone-popup");
const zoneSearch = byId("timezone-search", "input");
const zoneOptions = byId("timezone-options");
const localZone = Intl.DateTimeFormat().resolvedOptions().timeZone;
const timeZones = [
  ...new Set(["UTC", localZone, ...Intl.supportedValuesOf("timeZone")]),
];
const zoneName = (zone: string) =>
  zone.replaceAll("_", " ").replaceAll("/", " / ");
const zoneCity = (zone: string) => zone.split("/").at(-1)!.replaceAll("_", " ");
let chartZone = "UTC";
let zoneClock: Intl.DateTimeFormat;
let zoneTimestamp: Intl.DateTimeFormat;

function setChartZone(zone: string) {
  const timeOptions: Intl.DateTimeFormatOptions = {
    timeZone: zone,
    hour: "2-digit",
    minute: "2-digit",
    hourCycle: "h23",
  };
  try {
    zoneClock = new Intl.DateTimeFormat("en-GB", timeOptions);
    zoneTimestamp = new Intl.DateTimeFormat("en-GB", {
      ...timeOptions,
      month: "short",
      day: "numeric",
      timeZoneName: "shortOffset",
    });
  } catch {
    return false;
  }
  chartZone = zone;
  write("timezone-name", zoneCity(zone));
  zoneToggle.setAttribute("aria-label", `Chart time zone · ${zoneName(zone)}`);
  byId("chart-date-basis").hidden = zone === "UTC";
  if (chartDay) drawHours(chartDay);
  void refreshWeekdays();
  return true;
}

function drawZoneOptions() {
  const query = zoneSearch.value.trim().toLowerCase();
  zoneOptions.replaceChildren();
  for (const zone of timeZones) {
    const name = zoneName(zone);
    const label =
      zone === localZone && zone !== "UTC" ? `Local time · ${name}` : name;
    if (
      !label.toLowerCase().includes(query) &&
      !zone.toLowerCase().includes(query)
    )
      continue;
    const button = document.createElement("button");
    button.type = "button";
    button.dataset.zone = zone;
    button.textContent = label;
    button.setAttribute("aria-pressed", String(zone === chartZone));
    zoneOptions.append(button);
  }
  if (!zoneOptions.childElementCount) {
    const empty = document.createElement("p");
    empty.className = "detail";
    empty.textContent = "No matching time zones";
    zoneOptions.append(empty);
  }
}

function closeZonePicker() {
  zonePopup.hidden = true;
  zoneToggle.setAttribute("aria-expanded", "false");
}

zoneToggle.addEventListener("click", () => {
  if (!zonePopup.hidden) {
    closeZonePicker();
    return;
  }
  zoneSearch.value = "";
  drawZoneOptions();
  zonePopup.hidden = false;
  zoneToggle.setAttribute("aria-expanded", "true");
  zoneOptions.scrollTop = 0;
  zoneSearch.focus();
});
zoneSearch.addEventListener("input", drawZoneOptions);
zoneOptions.addEventListener("click", (event) => {
  if (!(event.target instanceof HTMLButtonElement)) return;
  const zone = event.target.dataset.zone;
  if (!zone || !setChartZone(zone)) return;
  try {
    localStorage.setItem("ironwood-chart-time-zone", zone);
  } catch {
    /* Optional preference. */
  }
  closeZonePicker();
  zoneToggle.focus();
});
zonePicker.addEventListener("keydown", (event) => {
  if (zonePopup.hidden) return;
  if (event.key === "Escape") {
    event.preventDefault();
    closeZonePicker();
    zoneToggle.focus();
  } else if (event.target === zoneSearch && event.key === "ArrowDown") {
    event.preventDefault();
    zoneOptions.querySelector("button")?.focus();
  }
});
zonePicker.addEventListener("focusout", (event) => {
  if (
    event.relatedTarget instanceof Node &&
    !zonePicker.contains(event.relatedTarget)
  )
    closeZonePicker();
});
document.addEventListener("pointerdown", (event) => {
  if (event.target instanceof Node && !zonePicker.contains(event.target))
    closeZonePicker();
});
let savedZone = "UTC";
try {
  savedZone = localStorage.getItem("ironwood-chart-time-zone") ?? "UTC";
} catch {
  /* Browser storage can be disabled. */
}
if (!setChartZone(savedZone)) setChartZone("UTC");

async function loadArchive(year: number) {
  if (archivedYears.has(year) || loadingYears.has(year)) return;
  loadingYears.add(year);
  try {
    const response = await fetch(`/v1/activity/calendar/${year}`, {
      signal: AbortSignal.timeout(12000),
    });
    if (!response.ok) return;
    archivedYears.set(year, (await response.json()) as Day[]);
    if (viewActive()) drawCalendar();
  } catch {
    // A visible archive with missing data is retried on the next activity refresh.
  } finally {
    loadingYears.delete(year);
  }
}

const archiveObserver = new IntersectionObserver(
  (entries) => {
    if (!viewActive()) return;
    for (const entry of entries) {
      const year = Number((entry.target as HTMLElement).dataset.year);
      if (entry.isIntersecting) void loadArchive(year);
    }
  },
  { root: calendarScroll },
);

function yearGrid(year: number, activation: number, latest: number, breaks: number[]) {
  const panel = document.createElement("section");
  panel.className = "calendar-year";
  panel.dataset.year = String(year);
  panel.setAttribute("aria-label", String(year));
  const heading = document.createElement("span");
  heading.className = "calendar-year-label";
  heading.textContent = String(year);
  const months = document.createElement("div");
  months.className = "calendar-months";
  months.setAttribute("aria-hidden", "true");
  const grid = document.createElement("div");
  grid.className = "calendar-grid";
  panel.append(heading, months, grid);
  const first = yearStart(year);
  const end = yearStart(year + 1);
  const offset = (date(first).getUTCDay() + 6) % 7;
  for (let i = 0; i < offset; i++) grid.append(document.createElement("span"));

  for (let start = first; start < end; start += daySeconds) {
    const instant = date(start);
    if (instant.getUTCDate() === 1) {
      const label = document.createElement("span");
      label.textContent = month.format(instant);
      label.style.gridColumn = String(Math.floor(((start - first) / daySeconds + offset) / 7) + 1);
      months.append(label);
    }
    const day = calendarDays.get(start);
    if (!day) {
      const square = document.createElement("span");
      square.className = "calendar-future";
      const status = start < activation ? "Before Ironwood activation" :
        start > latest ? "No indexed data yet" : "Loading activity";
      square.title = `${longDate.format(instant)} · ${status}`;
      grid.append(square);
      continue;
    }
    const n = total(day);
    const button = document.createElement("button");
    button.type = "button";
    button.dataset.start = String(start);
    button.dataset.level = String(n ? 1 + breaks.filter((limit) => n > limit).length : 0);
    button.title = `${longDate.format(instant)} · ${number.format(n)} transactions`;
    button.setAttribute("aria-label", button.title);
    grid.append(button);
  }
  return panel;
}

function drawCalendar() {
  if (!data) return;
  const days = data.days;
  const first = days[0], last = days.at(-1);
  archiveObserver.disconnect();
  calendarDays.clear();
  if (!first || !last) {
    void showFlows();
    chartDay = undefined;
    selectedHour = undefined;
    write("hour-label", "Peak hour");
    for (const id of ["calendar", "hour-chart", "hour-targets"])
      byId(id).replaceChildren();
    for (const id of ["calendar-total", "day-total", "peak-hour"])
      write(id, "—");
    write("selected-date", "Daily activity");
    write("peak-detail", "—");
    write("hour-coverage", "");
    write("calendar-period", "Waiting for the first Ironwood block");
    return;
  }
  const activation = data.activation_day ?? first.start;
  for (const archive of archivedYears.values())
    for (const day of archive) {
      if (day.start >= activation && day.start <= last.start)
        calendarDays.set(day.start, day);
    }
  for (const day of days) calendarDays.set(day.start, day);

  const focused = calendar.contains(document.activeElement);
  const oldScroll = calendarScroll.scrollLeft;
  const currentYear = Math.max(new Date().getUTCFullYear(), yearOf(last.start));
  const counts = days.map(total).filter((n) => n > 0).sort((a, b) => a - b);
  const breaks = [0.25, 0.5, 0.75].map(
    (q) => counts[Math.max(0, Math.ceil(counts.length * q) - 1)] ?? 0,
  );
  const panels: HTMLElement[] = [];
  for (let year = yearOf(activation); year <= currentYear; year++) {
    const panel = yearGrid(year, activation, last.start, breaks);
    panels.push(panel);
    const firstIndexed = Math.max(yearStart(year), activation);
    if (firstIndexed < first.start && !archivedYears.has(year))
      archiveObserver.observe(panel);
  }
  calendar.replaceChildren(...panels);
  const currentDays = days.filter((day) => yearOf(day.start) === currentYear);
  write("calendar-total", currentDays.length
    ? number.format(currentDays.reduce((sum, day) => sum + total(day), 0)) : "—");
  write("calendar-period", currentDays.length
    ? `Jan 1 – Dec 31, ${currentYear} · ${currentDays.length} indexed days`
    : `Jan 1 – Dec 31, ${currentYear} · Awaiting indexed days`);
  const current = (selected === undefined ? undefined : calendarDays.get(selected)) ?? last;
  showDay(current);
  if (viewActive() && calendarYear !== currentYear) {
    calendarYear = currentYear;
    const panel = panels.at(-1);
    if (panel) {
      const left = panel.getBoundingClientRect().left -
        calendarScroll.getBoundingClientRect().left + calendarScroll.scrollLeft;
      calendarScroll.scrollLeft = left;
      const selectedButton = panel.querySelector('[aria-pressed="true"]');
      const cell = selectedButton?.getBoundingClientRect();
      const viewport = calendarScroll.getBoundingClientRect();
      if (cell && cell.right > viewport.right)
        calendarScroll.scrollLeft += cell.right - viewport.right + 4;
    }
  } else {
    calendarScroll.scrollLeft = oldScroll;
  }
  if (focused)
    calendar.querySelector<HTMLButtonElement>('[aria-pressed="true"]')
      ?.focus({ preventScroll: true });
}

calendar.addEventListener("click", (e) => {
  if (!(e.target instanceof HTMLButtonElement)) return;
  const day = calendarDays.get(Number(e.target.dataset.start));
  if (day) showDay(day);
});
calendar.addEventListener("keydown", (e) => {
  const delta: Record<string, number> = {
    ArrowUp: -1,
    ArrowDown: 1,
    ArrowLeft: -7,
    ArrowRight: 7,
  };
  const buttons = Array.from(calendar.querySelectorAll("button"));
  const offset = delta[e.key];
  if (offset === undefined && e.key !== "Home" && e.key !== "End") return;
  const index = buttons.indexOf(document.activeElement as HTMLButtonElement);
  if (index < 0) return;
  e.preventDefault();
  const next =
    e.key === "Home"
      ? 0
      : e.key === "End"
        ? buttons.length - 1
        : index + (offset ?? 0);
  const button = buttons[Math.max(0, Math.min(buttons.length - 1, next))];
  button?.click();
  button?.focus();
});
function renderCommitments(d: Activity) {
  const c = d.commitments;
  write("calendar-commitments", number.format(c.from_calendar));
  write("coinbase-commitments", number.format(c.from_coinbase));
  write("earlier-commitments", number.format(c.from_earlier));
  byId("earlier-commitments-row").hidden = c.from_earlier === 0;
  write("tree-commitments", number.format(c.leaves));
  write("commitment-transactions", `From ${transactionCount(c.transactions)}`);
  write("commitment-block", `At block ${number.format(d.checkpoint.height)}`);
  const sizes = c.most_common.map((n) => number.format(n)).join(" or ");
  write(
    "commitment-detail",
    sizes
      ? `Most common · ${sizes} ${sizes === "1" ? "commitment" : "commitments"} per transaction`
      : "",
  );
}

function renderWeekdays(weekdays: WeekdaySummary[]) {
  const best = Math.max(...weekdays.map((w) => w.median ?? -1));
  const leaders = weekdays.flatMap((summary, index) =>
    summary.median === best
      ? [{ name: weekdayNames[index], hour: summary.peak_hour }]
      : [],
  );
  write(
    "weekday",
    leaders.length ? leaders.map((leader) => leader.name).join(" / ") : "—",
  );
  const peak = leaders[0]?.hour;
  write(
    "weekday-hour",
    peak != null && leaders.every((leader) => leader.hour === peak)
      ? `${hour(peak)}:00–${hour(peak + 1)}:00`
      : "",
  );
  const bars = byId("weekday-bars");
  bars.replaceChildren();
  weekdays.forEach((w, i) => {
    const name = weekdayNames[i]!;
    const item = document.createElement("div");
    const meter = document.createElement("meter");
    meter.min = 0;
    meter.max = Math.max(1, best);
    meter.value = w.median ?? 0;
    const text = `${name} · ${w.median === null ? "Awaiting complete days" : number.format(w.median) + " median transactions"} · ${w.days} days`;
    meter.setAttribute("aria-label", text);
    item.title = text;
    if (w.median === best) item.className = "top";
    item.append(meter, name.slice(0, 3));
    bars.append(item);
  });
  write(
    "weekday-detail",
    best >= 0
      ? `${number.format(best)} transactions on the median day · Last 365 days`
      : "Awaiting a complete day.",
  );
}

async function refreshWeekdays() {
  if (!data) return;
  const zone = chartZone;
  if (weekdayZone !== zone) {
    weekdayRequest?.abort();
    weekdayRequest = undefined;
    weekdayZone = zone;
    write("weekday-zone", zoneCity(zone));
    byId("weekday-zone").title = zone;
    if (zone !== "UTC") {
      write("weekday", "—");
      write("weekday-hour", "");
      byId("weekday-bars").replaceChildren();
      write("weekday-detail", "Calculating local weekday activity…");
    }
  }
  if (zone === "UTC") {
    renderWeekdays(data.weekdays);
    return;
  }
  if (weekdayRequest) return;
  const request = new AbortController();
  weekdayRequest = request;
  try {
    const response = await fetch(
      `/v1/activity/weekdays?zone=${encodeURIComponent(zone)}`,
      {
        cache: "no-store",
        signal: AbortSignal.any([request.signal, AbortSignal.timeout(12000)]),
      },
    );
    if (!response.ok) throw new Error("Local weekday activity unavailable");
    const weekdays = (await response.json()) as WeekdaySummary[];
    if (!request.signal.aborted) renderWeekdays(weekdays);
  } catch {
    if (!request.signal.aborted)
      write("weekday-detail", "Local weekday activity unavailable · Retrying");
  } finally {
    if (weekdayRequest === request) weekdayRequest = undefined;
  }
}

function render(d: Activity) {
  if (selected === data?.days.at(-1)?.start) selected = undefined;
  data = d;
  drawCalendar();
  renderCommitments(d);
  void refreshWeekdays();
  const lastHour = (Math.floor(d.checkpoint.time / 3600) - 1) * 3600;
  const lastDayStart = Math.floor(lastHour / 86400) * 86400;
  const currentDay = d.days.find((day) => day.start === lastDayStart);
  const h = Math.floor(lastHour / 3600) % 24;
  const reference = d.days
    .filter(
      (day) =>
        day.start < lastDayStart &&
        day.start >= lastDayStart - 28 * 86400 &&
        day.start !== d.days[0]?.start,
    )
    .map((day) => day.hours[h]!)
    .sort((a, b) => a - b);
  const typical = reference.length
    ? (reference[Math.floor(reference.length / 2)]! +
        reference[Math.floor((reference.length - 1) / 2)]!) /
      2
    : null;
  write("last-hour", currentDay ? number.format(currentDay.hours[h]!) : "—");
  write(
    "hour-baseline",
    `${hour(h)}:00–${hour(h + 1)}:00 UTC` +
      (typical === null
        ? ""
        : ` · Typically ${number.format(typical)} transactions`),
  );
  const load = d.load;
  write(
    "block-load",
    load.mean_actions === null
      ? "—"
      : load.capacity
        ? number.format((load.mean_actions / load.capacity) * 100) + "% used"
        : number.format(load.mean_actions) + " actions",
  );
  write(
    "load-detail",
    load.blocks === 100
      ? "Average over the last 100 blocks"
      : load.blocks
        ? `Average over ${load.blocks} recent blocks`
        : "Awaiting block data",
  );
  write(
    "pool-balance",
    d.balance ? number.format(d.balance.amount_zat / 1e8) + " ZEC" : "—",
  );
  write(
    "balance-detail",
    d.balance
      ? "At block " + number.format(d.balance.height)
      : "Awaiting pool balance",
  );
  write(
    "pending-count",
    d.pending.fresh && d.pending.transactions !== null
      ? number.format(d.pending.transactions)
      : "—",
  );
  write(
    "pending-detail",
    d.pending.fresh && d.pending.observed_at
      ? "Observed at " + clock(d.pending.observed_at)
      : "Awaiting a fresh mempool sample",
  );
  write("first-time", duration(d.first_confirmation.mean));
  write(
    "first-detail",
    d.first_confirmation.count
      ? `90% within ${duration(d.first_confirmation.p90)} · ${number.format(d.first_confirmation.count)} tracked`
      : "Waiting for a pending transaction to confirm",
  );
  write("ten-time", duration(d.ten_confirmations.median));
  write(
    "ten-detail",
    d.ten_confirmations.count
      ? `90% within ${duration(d.ten_confirmations.p90)} · Last 7 days`
      : "Awaiting ten blocks",
  );
  const stamp = date(d.checkpoint.time);
  const blockDate =
    stamp.toISOString().slice(0, 10) === new Date().toISOString().slice(0, 10)
      ? ""
      : shortDate.format(stamp) + " ";
  write(
    "connection",
    `${d.network === "main" ? "Mainnet" : d.network} · Block ${number.format(d.checkpoint.height)} · ${d.caught_up ? "Synced" : "Indexed"} ${blockDate}${clock(d.checkpoint.time)}`,
  );
}

let interval = 15000;
let refreshAfter = 0;
async function refresh() {
  if (requestPending || !viewActive()) return;
  requestPending = true;
  try {
    const res = await fetch("/v1/activity", {
      cache: "no-store",
      signal: AbortSignal.timeout(12000),
    });
    if (!res.ok) {
      if (res.status === 429)
        interval = Math.max(
          15000,
          Number(res.headers.get("Retry-After") || 15) * 1000,
        );
      throw new Error("Activity unavailable");
    }
    render((await res.json()) as Activity);
    interval = 15000;
  } catch {
    write(
      "connection",
      data
        ? "Connection interrupted · Last indexed " +
            shortDate.format(date(data.checkpoint.time)) +
            " " +
            clock(data.checkpoint.time)
        : "Activity data unavailable · Retrying",
    );
  } finally {
    requestPending = false;
  }
}
let timer: ReturnType<typeof setTimeout>;
async function poll() {
  clearTimeout(timer);
  if (!viewActive() || requestPending) return;
  if (Date.now() >= refreshAfter) {
    await refresh();
    refreshAfter = Date.now() + interval;
  }
  if (viewActive()) timer = setTimeout(poll, Math.max(0, refreshAfter - Date.now()));
}
document.addEventListener("visibilitychange", () => void poll());
document.addEventListener("ironwood-viewchange", () => void poll());
void poll();
