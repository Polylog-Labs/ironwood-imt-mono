import { test } from "node:test";
import assert from "node:assert/strict";
import { summarizeThroughput, type Day } from "../web/activity-model.ts";

const first = Date.UTC(2026, 10, 19) / 1000;
const day = (start: number, hourly: number): Day => ({ start, hours: Array(24).fill(hourly) });

test("daily TPS uses the selected day and includes its zero-transaction hours", () => {
  const selected = day(first, 0);
  selected.hours[7] = 3600;
  const result = summarizeThroughput([selected, day(first + 86400, 7200)], first, first + 86400)!;
  assert.equal(result.transactions, 3600);
  assert.equal(result.average, 1 / 24);
  assert.equal(result.peak, 1);
  assert.equal(result.peakHour, first + 7 * 3600);
});

test("an unfinished day excludes the current hour from both numerator and denominator", () => {
  const selected = day(first, 360);
  selected.hours[10] = 900_000;
  const result = summarizeThroughput([selected], first, first + 10.5 * 3600)!;
  assert.equal(result.transactions, 3600);
  assert.equal(result.average, 0.1);
  assert.equal(result.peak, 0.1);
  assert.equal(result.end, first + 10 * 3600);
});

test("all-time TPS weights complete hours across dates and keeps the earliest tied peak", () => {
  const earlier = day(first, 360);
  const later = day(first + 86400, 720);
  earlier.hours[23] = 3600;
  later.hours[0] = 3600;
  const result = summarizeThroughput([later, earlier], first, first + 86400 + 2.5 * 3600)!;
  assert.equal(result.transactions, 23 * 360 + 3600 + 3600 + 720);
  assert.equal(result.average, result.transactions / (26 * 3600));
  assert.equal(result.peak, 1);
  assert.equal(result.peakHour, first + 23 * 3600);
});

test("missing history stays unknown while a fully indexed empty day has zero TPS", () => {
  const empty = day(first, 0);
  assert.equal(summarizeThroughput([empty], first, first + 2 * 86400), undefined);
  const missingHour = { start: first, hours: Array(23).fill(0) };
  assert.equal(summarizeThroughput([missingHour], first, first + 86400), undefined);
  const result = summarizeThroughput([empty], first, first + 86400)!;
  assert.equal(result.average, 0);
  assert.equal(result.peak, 0);
  assert.equal(result.peakHour, undefined);
});

test("partial boundaries and empty measurement windows cannot inflate TPS", () => {
  const selected = day(first, 3600);
  assert.equal(summarizeThroughput([selected], first, first + 3599), undefined);
  assert.equal(summarizeThroughput([selected], first, first), undefined);
  assert.equal(summarizeThroughput([selected], first + 3600, first), undefined);
  const result = summarizeThroughput([selected], first + 1, first + 7200)!;
  assert.equal(result.start, first + 3600);
  assert.equal(result.transactions, 3600);
  assert.equal(result.average, 1);
});
