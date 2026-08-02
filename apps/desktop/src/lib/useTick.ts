import { useEffect, useState } from "react";

/** How often an elapsed clock or an estimate redraws while a job runs. */
const TICK_MS = 1_000;

/**
 * A clock that only runs when something on screen depends on it.
 *
 * Left running unconditionally it would re-render a whole history list once a
 * second forever, including in the tray with no window open.
 */
export function useTick(enabled: boolean): number {
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (!enabled) return;
    setNow(Date.now());
    const id = setInterval(() => setNow(Date.now()), TICK_MS);
    return () => clearInterval(id);
  }, [enabled]);

  return now;
}
