// Tiny reusable inline spinner for in-flight action buttons.
// Mirrors the desktop `.btn-pending::after` affordance so the two UIs
// give the same "this button is working" feedback. Style lives in
// styles.css under `.btn-spinner`.

export function Spinner() {
  return <span className="btn-spinner" aria-hidden="true" />;
}
