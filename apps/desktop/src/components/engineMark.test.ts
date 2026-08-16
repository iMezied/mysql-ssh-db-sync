import { describe, expect, it } from "vitest";

import { ENGINE_PATH, ENGINE_VIEWBOX } from "./EngineMark";
import { ENGINE_COLOR, ENGINE_LABEL } from "@/lib/engineDefaults";
import type { Engine } from "@/bindings";

/**
 * Guards for artwork that was generated rather than written.
 *
 * The three `d` attributes were interpolated from the published Simple Icons
 * files, which is the only safe way to move 9KB of path data — but it also
 * means nobody reviewing the diff can tell a dolphin from a smear. These
 * assertions cover the failures that would survive review and typechecking:
 * a truncated path, the same icon pasted under three names, or a colour left
 * at a placeholder.
 *
 * Exhaustiveness itself is a compile error — `Record<Engine, …>` sees to that.
 * The runtime key check is here for the other half of it: a fourth engine
 * silently stubbed with an empty string to make the compiler quiet.
 */

const ENGINES: Engine[] = ["mysql", "postgres", "mongo"];

describe("engine artwork", () => {
  it("covers exactly the engines the app supports", () => {
    expect(Object.keys(ENGINE_PATH).sort()).toEqual([...ENGINES].sort());
    expect(Object.keys(ENGINE_COLOR).sort()).toEqual([...ENGINES].sort());
    expect(Object.keys(ENGINE_LABEL).sort()).toEqual([...ENGINES].sort());
  });

  it.each(ENGINES)("%s has a path that looks like SVG geometry", (engine) => {
    const d = ENGINE_PATH[engine];
    // Every Simple Icons path opens with an absolute moveto and closes a
    // subpath. A truncated copy fails the second half.
    expect(d.startsWith("M")).toBe(true);
    expect(d.trimEnd().endsWith("z")).toBe(true);
    expect(d.length).toBeGreaterThan(200);
    // Quotes would mean the interpolation escaped its string literal.
    expect(d).not.toContain('"');
  });

  it("draws a different shape for each engine", () => {
    const paths = ENGINES.map((e) => ENGINE_PATH[e]);
    expect(new Set(paths).size).toBe(ENGINES.length);
  });

  it("gives each engine its own colour, light enough for a dark panel", () => {
    const colours = ENGINES.map((e) => ENGINE_COLOR[e]);
    expect(new Set(colours).size).toBe(ENGINES.length);

    for (const hex of colours) {
      expect(hex).toMatch(/^#[0-9A-Fa-f]{6}$/);
      // Relative luminance well above the slate-900 the panels are painted in.
      // A brand value dropped straight in would land near 0.1 and disappear.
      const [r, g, b] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255);
      const lin = (c: number) =>
        c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
      const luminance = 0.2126 * lin(r!) + 0.7152 * lin(g!) + 0.0722 * lin(b!);
      expect(luminance).toBeGreaterThan(0.25);
    }
  });

  it.each(ENGINES)("%s is framed by four finite numbers", (engine) => {
    const box = ENGINE_VIEWBOX[engine].split(" ").map(Number);
    expect(box).toHaveLength(4);
    expect(box.every(Number.isFinite)).toBe(true);
    // Width and height, which a zero would collapse into an invisible mark.
    expect(box[2]).toBeGreaterThan(0);
    expect(box[3]).toBeGreaterThan(0);
  });

  it("crops MySQL to the dolphin, above the wordmark", () => {
    // The lockup's letters sit on a baseline at y≈18.7 and rise to y≈13.2.
    // If the box ever reaches them again the mark turns to mush at 14px.
    const [, y, , height] = ENGINE_VIEWBOX.mysql.split(" ").map(Number);
    expect(y! + height!).toBeLessThan(13.2);
  });

  it("names each engine the way its own project does", () => {
    expect(ENGINE_LABEL.mysql).toBe("MySQL");
    expect(ENGINE_LABEL.postgres).toBe("PostgreSQL");
    expect(ENGINE_LABEL.mongo).toBe("MongoDB");
  });
});
