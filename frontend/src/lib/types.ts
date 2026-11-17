/** Shared view-model types for the Shorten console pages. */

export type Status = 'active' | 'expired' | 'disabled';

export interface ShortLink {
  slug: string;
  shortUrl: string;
  destination: string;
  title?: string;
  status: Status;
  visits: number;
  updated: string;
  created?: string;
  expiresAt?: string;
  redirectStatus?: '301' | '302' | '307' | '308';
  notes?: string;
}

export interface Column {
  label: string;
  width?: string;
  cellClass?: string;
  srOnly?: boolean;
}

export interface MetaEntry {
  term: string;
  detail: string;
}

/** Link form view-model shared by the create and edit flows. */
export interface LinkFormValues {
  destination: string;
  slug: string;
  title: string;
  status: Status;
  redirectStatus: '301' | '302' | '307' | '308';
  expiresAt: string;
  notes: string;
}

/** Server-side validation messages keyed by form field. */
export interface LinkFormErrors {
  destination?: string;
  slug?: string;
  title?: string;
  expiresAt?: string;
  notes?: string;
  redirectStatus?: string;
}

/** Counts render with tabular numerals: 12481 -> "12,481". */
export function formatCount(value: number): string {
  return value.toLocaleString('en-US');
}

/** Lifecycle label + class, mirroring the Go `statusLabel`/`statusClass` helpers. */
export function statusLabel(status: Status): string {
  return status === 'active' ? 'Active' : status === 'expired' ? 'Expired' : 'Disabled';
}
