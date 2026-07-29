/**
 * Shared building blocks for the Open Graph cards rendered by
 * `opengraph-image.tsx` routes.
 *
 * These render through Satori, not the browser, which constrains them:
 * - flexbox only, no grid
 * - no CSS variables, so the brand tokens are duplicated here as literals
 * - every element with more than one child needs an explicit `display`
 *
 * That last rule is the one that bites: `@{name}` compiles to two text
 * children and throws. Interpolate into a single template literal instead.
 */

export const OG_SIZE = { width: 1200, height: 630 } as const;

export const OG_CANVAS = "#0d1018";
export const OG_SURFACE = "#131822";
export const OG_BORDER = "rgba(255, 255, 255, 0.09)";
export const OG_TEXT = "#f4f7fb";
export const OG_TEXT_MUTED = "#a8b3c5";
export const OG_ACCENT = "#2f8fff";

export function OgStat({ label, value }: { label: string; value: string }) {
  return (
    <div
      style={{
        display: "flex",
        flexDirection: "column",
        flex: 1,
        padding: "28px 32px",
        border: `1px solid ${OG_BORDER}`,
        borderRadius: 20,
        background: OG_SURFACE,
      }}
    >
      <div style={{ fontSize: 56, fontWeight: 700, color: OG_TEXT, lineHeight: 1.1 }}>
        {value}
      </div>
      <div
        style={{
          marginTop: 10,
          fontSize: 22,
          color: OG_TEXT_MUTED,
          letterSpacing: 2,
        }}
      >
        {label}
      </div>
    </div>
  );
}

/**
 * Canvas, brand rule, and the tokscale.ai watermark. `children` fills the
 * space between them; pass a flex-grow spacer to bottom-align a stats row.
 */
export function OgCardShell({ children }: { children: React.ReactNode }) {
  return (
    <div
      style={{
        width: "100%",
        height: "100%",
        display: "flex",
        flexDirection: "column",
        background: OG_CANVAS,
        padding: 72,
      }}
    >
      <div
        style={{
          display: "flex",
          width: 120,
          height: 8,
          borderRadius: 4,
          background: OG_ACCENT,
        }}
      />

      {children}

      <div
        style={{
          display: "flex",
          justifyContent: "flex-end",
          marginTop: 32,
          fontSize: 26,
          color: OG_TEXT_MUTED,
        }}
      >
        tokscale.ai
      </div>
    </div>
  );
}
