import { describe, expect, it } from "vitest";
import { MAX_EXPANSION, expandAll, expandPattern, hasPattern } from "./pattern";

describe("expandPattern", () => {
  it("leaves a plain URL alone", () => {
    expect(expandPattern("https://e.test/a.zip")).toEqual(["https://e.test/a.zip"]);
    expect(hasPattern("https://e.test/a.zip")).toBe(false);
  });

  it("expands a range inclusively", () => {
    expect(expandPattern("https://e.test/f[1-3].jpg")).toEqual([
      "https://e.test/f1.jpg",
      "https://e.test/f2.jpg",
      "https://e.test/f3.jpg",
    ]);
  });

  /// The whole reason the width is read off the text: servers name files
  /// `img007.jpg`, and a batch that asks for `img7.jpg` gets 404s.
  it("keeps the zero padding the user wrote", () => {
    expect(expandPattern("https://e.test/img[008-010].jpg")).toEqual([
      "https://e.test/img008.jpg",
      "https://e.test/img009.jpg",
      "https://e.test/img010.jpg",
    ]);
  });

  it("counts down when the range does", () => {
    expect(expandPattern("https://e.test/[3-1].bin")).toEqual([
      "https://e.test/3.bin",
      "https://e.test/2.bin",
      "https://e.test/1.bin",
    ]);
  });

  it("treats a single-value range as one link", () => {
    expect(expandPattern("https://e.test/f[5-5].jpg")).toEqual(["https://e.test/f5.jpg"]);
  });

  /// Truncating would queue a batch quietly missing most of its files.
  it("refuses an expansion past the cap", () => {
    expect(() => expandPattern(`https://e.test/f[1-${MAX_EXPANSION + 1}].jpg`)).toThrow(
      /limit is 1000/,
    );
  });

  it("expands a list in order", () => {
    expect(expandAll(["https://e.test/a[1-2].bin", "https://e.test/b.bin"])).toEqual([
      "https://e.test/a1.bin",
      "https://e.test/a2.bin",
      "https://e.test/b.bin",
    ]);
  });
});
