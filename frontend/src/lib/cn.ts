/**
 * `cn` — minimal class-name joiner for the Shorten primitives.
 *
 * shadcn uses `clsx` + `tailwind-merge`; this project deliberately ships no
 * runtime dependencies, so the primitives compose the accepted design-system
 * class names (`.button`, `.button-primary`, `.control`, `.field`, …) by
 * concatenation. Falsy values are dropped so variants can be expressed as
 * `wide && 'field-wide'`.
 */
export type ClassValue = string | number | null | false | undefined | ClassValue[];

export function cn(...values: ClassValue[]): string {
  const out: string[] = [];

  const collect = (value: ClassValue): void => {
    if (Array.isArray(value)) {
      for (const item of value) collect(item);
      return;
    }
    if (!value && value !== 0) return;
    out.push(String(value));
  };

  for (const value of values) collect(value);

  return out.join(' ').trim();
}

/**
 * Attribute bag for `set:attributes` — every primitive forwards its remaining
 * props (aria-*, data-*, hx-*, name, id, …) straight to the root element so the
 * HTMX attributes stay writable exactly as the Go/Askama templates write them.
 */
export type Attr = string | number | boolean | null | undefined;
export type AttrBag = Record<string, Attr>;

/**
 * Slot content that Astro hands to a component as the `children` prop.
 * Primitives accept it in their `Props` so the index-signature attribute bag
 * stays type-checked (`astro check`) while slotted markup remains valid.
 */
export type Children = astroHTML.JSX.Children;
