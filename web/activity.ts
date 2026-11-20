import { byId } from "./dom.ts";
import { summarizeThroughput, type Day } from "./activity-model.ts";

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
  flow_history?: { transactions: number; decoded: number };
  flow_worker?: "idle" | "decoding" | "retrying";
  weekdays: WeekdaySummary[];
  commitments: {
    from_calendar: number;
    from_coinbase: number;
    from_earlier: number;
    leaves: number;
    transactions: number;
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
interface PoolFlows {
  transactions: number;
  checked: number;
  in_count: number;
  out_count: number;
  total_in_zat: number;
  total_out_zat: number;
  median_in_zat: number | null;
  median_out_zat: number | null;
}

const number = new Intl.NumberFormat("en-US", { maximumFractionDigits: 1 });
const amount = new Intl.NumberFormat("en-US", { maximumFractionDigits: 9 });
const roundedAmount = new Intl.NumberFormat("en-US", { maximumFractionDigits: 2 });
const tps = new Intl.NumberFormat("en-US", { maximumSignificantDigits: 3 });
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
let weekdaySummaries: WeekdaySummary[] = [];
let selectedWeekday: number | undefined;
const calendar = byId("calendar");
const calendarScroll = byId("calendar-scroll");
const daySeconds = 86400;
const yearOf = (start: number) => date(start).getUTCFullYear();
const yearStart = (year: number) => Date.UTC(year, 0, 1) / 1000;
const calendarDays = new Map<number, Day>();
const archivedYears = new Map<number, Day[]>();
const loadingYears = new Set<number>();
let calendarYear: number | undefined;
let calendarWidth = 0;
const poolPage = byId("pool-view");
const viewActive = () => !poolPage.hidden && !document.hidden;
const svgNS = "http://www.w3.org/2000/svg";

const dailyFlows = byId("flow-daily", "button");
const allTimeFlows = byId("flow-all-time", "button");
let allTime = false;
let loadedFlowScope: number | "all-time" | undefined;
let flowRequest: AbortController | undefined;
function writeFlowTotal(id: string, zat?: number) {
  const element = byId(id);
  if (zat === undefined) {
    element.textContent = "—";
    element.removeAttribute("title");
    return;
  }
  const zec = zat / 1e8;
  element.textContent = (zec < 1 ? amount : roundedAmount).format(zec);
  element.title = `${amount.format(zec)} ZEC`;
}

function renderFlows(flows?: PoolFlows) {
  const formatMedian = (zat?: number | null) =>
    zat == null ? "—" : amount.format(zat / 1e8);
  writeFlowTotal("total-in", flows?.total_in_zat);
  writeFlowTotal("total-out", flows?.total_out_zat);
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

async function showFlows() {
  const scope = allTime ? "all-time" : selected;
  const period = scope === "all-time" ? "All indexed history"
    : scope === undefined ? ""
    : `${shortDate.format(date(scope))}, ${yearOf(scope)} UTC`;
  dailyFlows.disabled = selected === undefined;
  dailyFlows.setAttribute("aria-pressed", String(!allTime));
  allTimeFlows.setAttribute("aria-pressed", String(allTime));
  if (loadedFlowScope !== scope) {
    flowRequest?.abort();
    flowRequest = undefined;
    loadedFlowScope = scope;
    write("flow-date", period ? `${period} · ZEC` : "ZEC");
    renderFlows();
  }
  if (scope === undefined || flowRequest) return;
  const request = new AbortController();
  flowRequest = request;
  try {
    const response = await fetch(`/v1/activity/flows/${scope}`, {
      cache: "no-store",
      signal: AbortSignal.any([request.signal, AbortSignal.timeout(12000)]),
    });
    if (!response.ok) throw new Error("Pool flows unavailable");
    const flows = (await response.json()) as PoolFlows;
    if (request.signal.aborted) return;
    renderFlows(flows);
    const partial = flows.checked < flows.transactions;
    write("flow-date", partial && scope === "all-time" ? "Decoded history · ZEC"
      : `${period} · ZEC${partial ? " · Partial" : ""}`);
  } catch {
    if (!request.signal.aborted)
      write("flow-date", "Pool flows unavailable · Retrying");
  } finally {
    if (flowRequest === request) flowRequest = undefined;
  }
}

function setPoolScope(history: boolean) {
  allTime = history;
  if (allTime) {
    selectedWeekday = undefined;
    if (weekdayBars.childElementCount) showWeekdayDetail();
    weekdayReset.disabled = true;
  }
  void showFlows();
  renderThroughput();
}
dailyFlows.addEventListener("click", () => setPoolScope(false));
allTimeFlows.addEventListener("click", () => setPoolScope(true));

function showDay(day: Day) {
  selected = day.start;
  void showFlows();
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
  renderThroughput();
}

const hourTargets = byId("hour-targets");
const hourScale = byId("hour-scale", "button");
let logarithmicHours = false;
try {
  logarithmicHours = localStorage.getItem("ironwood-hour-scale") === "log";
} catch {
  /* Optional preference. */
}
hourScale.setAttribute("aria-pressed", String(logarithmicHours));
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
    const ratio = logarithmicHours
      ? Math.log1p(count) / Math.log1p(peak)
      : count / peak;
    const height = Math.max(1, ratio * 78);
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

hourScale.addEventListener("click", () => {
  logarithmicHours = !logarithmicHours;
  hourScale.setAttribute("aria-pressed", String(logarithmicHours));
  try {
    localStorage.setItem("ironwood-hour-scale", logarithmicHours ? "log" : "linear");
  } catch {
    /* Optional preference. */
  }
  if (chartDay) drawHours(chartDay);
});

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
const cityAliases = new Map([
  ["Asia/Kolkata", { label: "Delhi, Mumbai", search: "new delhi calcutta india" }],
  ["Asia/Shanghai", { label: "Beijing", search: "bejing" }],
  ["Asia/Tokyo", { label: "", search: "tokio" }],
  ["America/New_York", { label: "Miami", search: "nyc new york city" }],
]);
const canonicalZone = (zone: string) =>
  zone === "Asia/Calcutta" ? "Asia/Kolkata" : zone;
const localZone = canonicalZone(Intl.DateTimeFormat().resolvedOptions().timeZone);
const timeZones = [
  ...new Set(
    ["UTC", localZone, ...Intl.supportedValuesOf("timeZone"), ...cityAliases.keys()]
      .map(canonicalZone),
  ),
];
const zoneName = (zone: string) => {
  const name = zone.replaceAll("_", " ").replaceAll("/", " / ");
  const aliases = cityAliases.get(zone)?.label;
  return aliases ? `${name} · ${aliases}` : name;
};
const zoneCity = (zone: string) =>
  zone === "Asia/Kolkata" ? "Delhi / Mumbai"
    : zone === "Asia/Shanghai" ? "Beijing / Shanghai"
    : zone.split("/").at(-1)!.replaceAll("_", " ");
let chartZone = "UTC";
let zoneClock: Intl.DateTimeFormat;
let zoneTimestamp: Intl.DateTimeFormat;

function setChartZone(zone: string) {
  zone = canonicalZone(zone);
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
      !zone.toLowerCase().includes(query) &&
      !cityAliases.get(zone)?.search.includes(query)
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

function revealCalendarSelection(panel: ParentNode) {
  const button = panel.querySelector('[aria-pressed="true"]');
  if (!button) return;
  const viewport = calendarScroll.getBoundingClientRect();
  const cell = button.getBoundingClientRect();
  if (cell.left < viewport.left || cell.right > viewport.right)
    calendarScroll.scrollLeft += cell.right - viewport.right + 16;
}

new ResizeObserver(() => {
  const width = calendarScroll.clientWidth;
  if (!viewActive() || width === 0 || width === calendarWidth) return;
  calendarWidth = width;
  revealCalendarSelection(calendar);
}).observe(calendarScroll);

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
  const launch = byId("ironwood-launch");
  launch.hidden = data.activation_day === null;
  launch.textContent = data.activation_day === null
    ? ""
    : `Ironwood launched ${shortDate.format(date(data.activation_day))}, ${yearOf(data.activation_day)}`;
  const days = data.days;
  const first = days[0], last = days.at(-1);
  archiveObserver.disconnect();
  calendarDays.clear();
  if (!first || !last) {
    selected = undefined;
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
    renderThroughput();
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
      revealCalendarSelection(matchMedia("(max-width: 780px)").matches ? calendar : panel);
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
  if (day) {
    allTime = false;
    showDay(day);
  }
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
  const days = Math.min(d.days.length, 365);
  write(
    "commitment-period",
    days ? `Commitments across ${days} indexed ${days === 1 ? "day" : "days"}`
      : "Commitments in indexed days",
  );
  write("calendar-commitments", number.format(c.from_calendar));
  write("coinbase-commitments", number.format(c.from_coinbase));
  write("earlier-commitments", number.format(c.from_earlier));
  byId("earlier-commitments-row").hidden = c.from_earlier === 0;
  write("tree-commitments", number.format(c.leaves));
  write("commitment-transactions", `From ${transactionCount(c.transactions)}`);
  write("commitment-block", `At block ${number.format(d.checkpoint.height)}`);
}

const weekdayBars = byId("weekday-bars");
const weekdayReset = byId("weekday-reset", "button");
const weekdayHourRange = (peak: number) =>
  `${hour(peak)}:00–${hour((peak + 1) % 24)}:00`;

function weekdayDetail(weekdays: WeekdaySummary[], selected?: number) {
  const completeDays = weekdays.reduce((sum, day) => sum + day.days, 0);
  const best = Math.max(...weekdays.map((w) => w.median ?? -1));
  const shown = weekdays.flatMap((summary, index) =>
    (selected === undefined
      ? summary.median === best
      : index === selected)
      ? [{ ...summary, name: weekdayNames[index]! }]
      : [],
  );
  const peak = shown[0]?.peak_hour;
  const day = selected === undefined ? undefined : shown[0];
  return {
    heading: selected === undefined ? "Busiest weekday" : "Selected weekday",
    name: shown.length ? shown.map((day) => day.name).join(" / ") : "—",
    peak: peak != null && shown.every((day) => day.peak_hour === peak)
      ? weekdayHourRange(peak)
      : "—",
    detail: day
      ? day.median === null
        ? `Awaiting a complete ${day.name}.`
        : `${number.format(day.median)} median transactions · ${day.days} complete ${day.name}${day.days === 1 ? "" : "s"}${day.peak_hour === null ? " · No typical peak hour" : ""}`
      : best >= 0
        ? `${number.format(best)} transactions on the median day · ${completeDays} complete indexed ${completeDays === 1 ? "day" : "days"}`
        : "Awaiting a complete day.",
  };
}

function showWeekdayDetail() {
  const summary = weekdayDetail(weekdaySummaries, selectedWeekday);
  write("weekday-heading", summary.heading);
  write("weekday", summary.name);
  write("weekday-hour", summary.peak);
  write("weekday-detail", summary.detail);
  weekdayReset.disabled = selectedWeekday === undefined;
  for (const button of weekdayBars.querySelectorAll("button")) {
    button.setAttribute(
      "aria-pressed",
      String(Number(button.dataset.weekday) === selectedWeekday),
    );
  }
}

weekdayReset.addEventListener("click", () => {
  selectedWeekday = undefined;
  showWeekdayDetail();
  weekdayBars.querySelector<HTMLButtonElement>(".top")?.focus({ preventScroll: true });
});

weekdayBars.addEventListener("click", (event) => {
  if (!(event.target instanceof Element)) return;
  const button = event.target.closest<HTMLButtonElement>("button[data-weekday]");
  if (!button || !weekdayBars.contains(button)) return;
  const weekday = Number(button.dataset.weekday);
  selectedWeekday = selectedWeekday === weekday ? undefined : weekday;
  showWeekdayDetail();
});

function renderWeekdays(weekdays: WeekdaySummary[]) {
  weekdaySummaries = weekdays;
  const best = Math.max(...weekdays.map((w) => w.median ?? -1));
  const active = document.activeElement;
  const focusedWeekday =
    active instanceof HTMLButtonElement && weekdayBars.contains(active)
      ? Number(active.dataset.weekday)
      : undefined;
  weekdayBars.replaceChildren();
  weekdays.forEach((w, i) => {
    const name = weekdayNames[i]!;
    const item = document.createElement("button");
    item.type = "button";
    item.dataset.weekday = String(i);
    const peak = w.peak_hour === null
      ? "No typical peak hour"
      : `Typical peak hour ${weekdayHourRange(w.peak_hour)} ${weekdayZone}`;
    const text = `${name} · ${w.median === null ? "Awaiting complete days" : number.format(w.median) + " median transactions"} · ${w.days} complete days · ${peak}`;
    item.setAttribute("aria-label", text);
    const bar = document.createElement("span");
    bar.className = "weekday-bar";
    bar.style.setProperty(
      "--level",
      `${((w.median ?? 0) / Math.max(1, best)) * 100}%`,
    );
    bar.setAttribute("aria-hidden", "true");
    const label = document.createElement("span");
    label.textContent = name.slice(0, 3);
    item.title = text;
    if (w.median === best) item.className = "top";
    item.append(bar, label);
    weekdayBars.append(item);
  });
  // Stack invisible variants in the same grid cells so selecting any day or
  // resetting cannot resize the card, including when text wraps on mobile.
  const summarySizes = byId("weekday-summary-sizes");
  const detailSizes = byId("weekday-detail-sizes");
  summarySizes.replaceChildren();
  detailSizes.replaceChildren();
  for (const weekday of [undefined, ...weekdays.keys()]) {
    const summary = weekdayDetail(weekdays, weekday);
    const sizing = byId("weekday-summary").cloneNode(true) as HTMLElement;
    sizing.querySelector("h2")!.textContent = summary.heading;
    sizing.querySelector("strong")!.textContent = summary.name;
    sizing.querySelector(".weekday-hour")!.textContent = summary.peak;
    sizing.removeAttribute("id");
    for (const node of sizing.querySelectorAll("[id]")) node.removeAttribute("id");
    sizing.querySelector('[role="status"]')?.removeAttribute("role");
    summarySizes.append(sizing);
    const detail = document.createElement("p");
    detail.className = "detail";
    detail.textContent = summary.detail;
    detailSizes.append(detail);
  }
  showWeekdayDetail();
  if (focusedWeekday !== undefined) {
    weekdayBars
      .querySelectorAll("button")
      [focusedWeekday]?.focus({ preventScroll: true });
  }
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
      weekdayReset.disabled = true;
      write("weekday", "—");
      write("weekday-hour", "—");
      weekdayBars.replaceChildren();
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

function renderThroughput() {
  write("tps-average-label", allTime ? "All-time average" : "Day average");
  for (const id of ["tps-hour", "tps-day"]) {
    write(id, "—");
    byId(id).removeAttribute("title");
  }
  if (!data || data.activation_day === null || selected === undefined) {
    write("tps-period", "Awaiting indexed history");
    return;
  }
  const dayLabel = (time: number) => `${shortDate.format(date(time))}, ${yearOf(time)}`;
  const from = allTime ? data.activation_day + daySeconds : selected;
  const to = allTime ? data.checkpoint.time : Math.min(selected + daySeconds, data.checkpoint.time);
  if (!allTime && selected === data.activation_day) {
    write("tps-period", `${dayLabel(selected)} UTC · Launch day is partial`);
    return;
  }
  if (Math.floor(to / 3600) * 3600 <= from) {
    write("tps-period", "Awaiting a complete indexed hour");
    return;
  }
  if (allTime) {
    const first = data.days[0]?.start ?? to;
    for (let year = yearOf(from); year <= yearOf(to - 1); year++) {
      if (Math.max(from, yearStart(year)) < first) void loadArchive(year);
    }
  }
  const summary = summarizeThroughput(calendarDays.values(), from, to);
  if (!summary) {
    write("tps-period", allTime ? "Loading complete indexed history…"
      : `${dayLabel(selected)} UTC · Awaiting a complete indexed hour`);
    return;
  }
  write("tps-hour", tps.format(summary.peak));
  write("tps-day", tps.format(summary.average));
  byId("tps-hour").title = summary.peakHour === undefined
    ? "No transactions in this period"
    : `${dayLabel(summary.peakHour)} · ${clock(summary.peakHour)} – ${clock(summary.peakHour + 3600)}`;
  byId("tps-day").title = `${transactionCount(summary.transactions)} across ${(summary.end - summary.start) / 3600} complete indexed hours`;
  const period = allTime
    ? `${dayLabel(summary.start)} – ${dayLabel(summary.end - 1)} UTC`
    : `${dayLabel(selected)} UTC`;
  write("tps-period", `${period} · ${allTime || summary.end < selected + daySeconds ? `Through ${clock(summary.end)}` : "24 complete hours"}`);
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
  const history = d.flow_history;
  const indicator = byId("flow-history");
  indicator.hidden = !history?.transactions;
  if (history?.transactions) {
    const complete = history.decoded === history.transactions;
    const label = complete ? "Historical flows decoded"
      : d.flow_worker === "retrying" ? "Historical flow decoding retrying"
      : d.flow_worker === "decoding" ? "Decoding historical flows"
      : "Historical flows incomplete";
    const progress = complete ? number.format(history.transactions)
      : `${number.format(history.decoded)} of ${number.format(history.transactions)}`;
    indicator.dataset.decoding = String(!complete && d.flow_worker === "decoding");
    const status = `${label} · ${progress} transactions`;
    if (indicator.textContent !== status) indicator.textContent = status;
  }
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
    byId("flow-history").hidden = true;
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
