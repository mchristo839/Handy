// FlowLite branding: shows "FlowLite" wherever upstream strings say "Handy",
// without editing the upstream translation files.
import i18n from "i18next";

type Tree = { [key: string]: string | Tree };

const rename = (node: Tree): Tree =>
  Object.fromEntries(
    Object.entries(node).map(([k, v]) => [
      k,
      typeof v === "string" ? v.replace(/\bHandy\b/g, "FlowLite") : rename(v),
    ]),
  );

const rebrand = () => {
  for (const [lng, namespaces] of Object.entries(i18n.store.data)) {
    for (const [ns, tree] of Object.entries(namespaces)) {
      i18n.addResourceBundle(lng, ns, rename(tree as Tree), true, true);
    }
  }
};

if (i18n.isInitialized) rebrand();
else i18n.on("initialized", rebrand);
