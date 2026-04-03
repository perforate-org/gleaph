export const siteSections = [
  "hero",
  "capabilities",
  "pricing",
  "faq",
] as const;

export function getPrimaryCallToAction() {
  return {
    label: "Open Dashboard",
    href: "/dashboard",
  };
}
