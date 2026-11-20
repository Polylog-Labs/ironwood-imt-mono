export interface Day {
  start: number;
  hours: number[];
}

// Only complete hours contribute to the average. Missing history is unknown,
// while an indexed hour containing no transactions contributes a real zero.
export function summarizeThroughput(days: Iterable<Day>, from: number, to: number) {
  const start = Math.ceil(from / 3600) * 3600;
  const end = Math.floor(to / 3600) * 3600;
  if (end <= start) return;
  const indexed = new Map(Array.from(days, (day) => [day.start, day]));
  let transactions = 0;
  let peakCount = 0;
  let peakHour: number | undefined;
  for (let time = start; time < end; time += 3600) {
    const dayStart = Math.floor(time / 86400) * 86400;
    const count = indexed.get(dayStart)?.hours[(time - dayStart) / 3600];
    if (count === undefined) return;
    transactions += count;
    if (count > peakCount) {
      peakCount = count;
      peakHour = time;
    }
  }
  return {
    start,
    end,
    transactions,
    average: transactions / (end - start),
    peak: peakCount / 3600,
    peakHour,
  };
}
