import { For, Show, createSignal } from "solid-js";

import { LOCALES, LOCALE_NAMES, useI18n, type Locale } from "~/i18n";

export function LanguageSwitcher() {
  const { locale, setLocale, t } = useI18n();
  const [open, setOpen] = createSignal(false);

  const selectLocale = (next: Locale) => {
    setLocale(next);
    setOpen(false);
  };

  return (
    <div
      class="relative"
      onFocusOut={(event) => {
        const next = event.relatedTarget as Node | null;
        if (!next || !(event.currentTarget as HTMLElement).contains(next)) {
          setOpen(false);
        }
      }}
    >
      <button
        type="button"
        class="inline-flex items-center gap-1.5 rounded-lg border border-slate-200 bg-white px-3 py-1.5 text-sm font-medium text-slate-700 shadow-sm transition hover:border-indigo-300 hover:text-indigo-700"
        aria-label={t("language.label")}
        aria-haspopup="menu"
        aria-expanded={open()}
        onClick={() => setOpen((value) => !value)}
      >
        <span>{LOCALE_NAMES[locale()]}</span>
        <span aria-hidden="true" class="text-xs">▾</span>
      </button>

      <Show when={open()}>
        <div
          class="absolute right-0 z-30 mt-2 min-w-32 rounded-lg border border-slate-200 bg-white p-1 shadow-lg"
          role="menu"
          aria-label={t("language.menuLabel")}
        >
          <For each={LOCALES}>
            {(option) => (
              <button
                type="button"
                role="menuitem"
                class="flex w-full items-center justify-between rounded-md px-3 py-2 text-left text-sm transition hover:bg-slate-100"
                classList={{
                  "bg-indigo-50 font-semibold text-indigo-700": locale() === option,
                  "text-slate-700": locale() !== option,
                }}
                aria-current={locale() === option ? "true" : undefined}
                onClick={() => selectLocale(option)}
              >
                <span>{LOCALE_NAMES[option]}</span>
                <Show when={locale() === option}>
                  <span aria-hidden="true">✓</span>
                </Show>
              </button>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}
