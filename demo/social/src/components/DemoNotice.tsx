import { useI18n } from "~/i18n";

export function DemoNotice() {
  const { t } = useI18n();

  return (
    <span class="rounded-full border border-amber-200 bg-amber-50 px-2.5 py-1 text-xs font-medium text-amber-800">
      {t("notice.anonymousReadOnly")}
    </span>
  );
}
