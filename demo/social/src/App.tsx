import { SocialDemo } from "~/components/SocialDemo";
import { I18nProvider } from "~/i18n";

export default function App() {
  return (
    <I18nProvider>
      <SocialDemo />
    </I18nProvider>
  );
}
