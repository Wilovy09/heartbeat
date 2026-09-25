// T('key', { name: value }): the client-side twin of the templates' t(k=...). Strings come
// from static/i18n/<lang>.js (window.HB_I18N); an unknown key renders as itself.
function T(key, vars = {}) {
  const template = (window.HB_I18N && window.HB_I18N[key]) || key;
  return template.replace(/\{(\w+)\}/g, (match, name) => (name in vars ? String(vars[name]) : match));
}
