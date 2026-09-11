// Snooze presets mirror T3's shared client logic
// (pingdotgg/t3code `threadSettled.ts`): local calendar boundaries,
// evening at 18:00, morning at 09:00, next week on Monday 09:00.

const HOUR_MS = 60 * 60 * 1_000;
const DAY_MS = 24 * 60 * 60 * 1_000;
const EVENING_HOUR = 18;
const MORNING_HOUR = 9;

export type SnoozePresetId = "hour" | "three-hours" | "evening" | "tomorrow" | "next-week";

export interface SnoozePreset {
  readonly id: SnoozePresetId;
  readonly label: string;
  readonly whenLabel: string;
  /** ISO wake time. */
  readonly snoozedUntil: string;
}

function timeOfDayLabel(date: Date): string {
  return date.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
}

function atHour(base: Date, hour: number): Date {
  const next = new Date(base);
  next.setHours(hour, 0, 0, 0);
  return next;
}

function addDays(base: Date, days: number): Date {
  const next = new Date(base);
  next.setDate(next.getDate() + days);
  return next;
}

/**
 * Shared "snooze until" choices. "This evening" only appears while it is
 * meaningfully before evening; after that the calendar choices start at
 * "Tomorrow". Presets that land on the same instant collapse.
 */
export function resolveSnoozePresets(now: Date = new Date()): ReadonlyArray<SnoozePreset> {
  const inAnHour = new Date(now.getTime() + HOUR_MS);
  const inThreeHours = new Date(now.getTime() + 3 * HOUR_MS);
  const presets: SnoozePreset[] = [
    { id: "hour", label: "In 1 hour", whenLabel: timeOfDayLabel(inAnHour), snoozedUntil: inAnHour.toISOString() },
    {
      id: "three-hours",
      label: "In 3 hours",
      whenLabel: timeOfDayLabel(inThreeHours),
      snoozedUntil: inThreeHours.toISOString(),
    },
  ];

  const evening = atHour(now, EVENING_HOUR);
  if (evening.getTime() - now.getTime() > HOUR_MS) {
    presets.push({
      id: "evening",
      label: "This evening",
      whenLabel: timeOfDayLabel(evening),
      snoozedUntil: evening.toISOString(),
    });
  }

  const tomorrow = atHour(addDays(now, 1), MORNING_HOUR);
  presets.push({
    id: "tomorrow",
    label: "Tomorrow",
    whenLabel: timeOfDayLabel(tomorrow),
    snoozedUntil: tomorrow.toISOString(),
  });

  const daysUntilMonday = (1 - now.getDay() + 7) % 7 || 7;
  const nextWeek = atHour(addDays(now, daysUntilMonday), MORNING_HOUR);
  if (nextWeek.getTime() !== tomorrow.getTime()) {
    presets.push({
      id: "next-week",
      label: "Next week",
      whenLabel: `${nextWeek.toLocaleDateString(undefined, { weekday: "short" })} ${timeOfDayLabel(nextWeek)}`,
      snoozedUntil: nextWeek.toISOString(),
    });
  }

  return presets;
}

/**
 * Default snooze for voice ("snooze this thread"): this evening while it is
 * meaningfully before evening, otherwise tomorrow morning.
 */
export function defaultSnoozePreset(now: Date = new Date()): SnoozePreset {
  const presets = resolveSnoozePresets(now);
  return presets.find((preset) => preset.id === "evening") ?? presets.find((preset) => preset.id === "tomorrow")!;
}

/** Human wake time for confirmations: "tomorrow 9:00 AM", "Mon 9:00 AM", "6:00 PM" (today). */
export function snoozeWakeDescription(snoozedUntil: string, now: Date = new Date()): string {
  const wake = new Date(snoozedUntil);
  if (Number.isNaN(wake.getTime())) return snoozedUntil;
  const time = timeOfDayLabel(wake);
  const startOfToday = new Date(now);
  startOfToday.setHours(0, 0, 0, 0);
  const dayDelta = Math.floor((wake.getTime() - startOfToday.getTime()) / DAY_MS);
  if (dayDelta <= 0) return time;
  if (dayDelta === 1) return `tomorrow ${time}`;
  const weekday = wake.toLocaleDateString(undefined, { weekday: "short" });
  if (dayDelta < 7) return `${weekday} ${time}`;
  return `${wake.toLocaleDateString(undefined, { month: "short", day: "numeric" })}, ${time}`;
}
