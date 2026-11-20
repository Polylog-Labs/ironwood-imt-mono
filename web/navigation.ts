interface Page {
  main: HTMLElement;
  title: string;
  scrollTop: number;
}

const views = {
  "/": { id: "pool-view", load: () => import("./activity.ts") },
  "/ironwood": { id: "tree-view", load: () => import("./dashboard.ts") },
};
type Path = keyof typeof views;
const pathFor = (pathname: string): Path | undefined =>
  pathname === "/" ? "/" :
    pathname === "/ironwood" || pathname === "/ironwood/" ? "/ironwood" : undefined;

const initialPath = pathFor(location.pathname);
const initialMain = document.querySelector("main");
const header = document.querySelector(".site-header");
const activityStyles = document.querySelector<HTMLLinkElement>('link[href="/activity.css"]');

if (initialPath && initialMain && header && activityStyles) {
  const siteHeader = header;
  const poolStyles = activityStyles;
  const pages = new Map<Path, Promise<Page>>();
  let activePath = initialPath;
  let activePage: Page = { main: initialMain, title: document.title, scrollTop: scrollY };
  let navigation = 0;
  pages.set(activePath, Promise.resolve(activePage));
  history.scrollRestoration = "manual";

  async function loadPage(path: Path): Promise<Page> {
    const response = await fetch(path);
    if (!response.ok) throw new Error("Page unavailable");
    const html = new DOMParser().parseFromString(await response.text(), "text/html");
    const main = html.querySelector<HTMLElement>(`main#${views[path].id}`);
    if (!main) throw new Error("Page content unavailable");
    main.hidden = true;
    document.body.insertBefore(main, activePage.main.nextSibling);
    return { main, title: html.title, scrollTop: 0 };
  }

  async function navigate(path: Path, push: boolean) {
    const id = ++navigation;
    if (path === activePath) return;
    let pending = pages.get(path);
    if (!pending) {
      pending = loadPage(path);
      pages.set(path, pending);
    }
    try {
      const page = await pending;
      if (id !== navigation) return;
      activePage.scrollTop = scrollY;
      activePage.main.hidden = true;
      activePath = path;
      activePage = page;
      poolStyles.media = path === "/" ? "all" : "not all";
      document.body.classList.toggle("activity-page", path === "/");
      page.main.hidden = false;
      document.title = page.title;
      for (const link of siteHeader.querySelectorAll<HTMLAnchorElement>("nav a")) {
        if (pathFor(link.pathname) === path) link.setAttribute("aria-current", "page");
        else link.removeAttribute("aria-current");
      }
      if (push) history.pushState(null, "", path);
      if (push) {
        const title = page.main.querySelector("h1");
        if (title) {
          title.tabIndex = -1;
          title.focus({ preventScroll: true });
        }
      }
      scrollTo(0, page.scrollTop);
      document.dispatchEvent(new Event("ironwood-viewchange"));
      await views[path].load();
    } catch {
      pages.delete(path);
      if (id === navigation) location.assign(path);
    }
  }

  header.addEventListener("click", event => {
    if (!(event instanceof MouseEvent) || event.defaultPrevented ||
        event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey ||
        !(event.target instanceof Element)) return;
    const link = event.target.closest<HTMLAnchorElement>("a[href]");
    if (!link || link.origin !== location.origin || link.target || link.search || link.hash) return;
    const path = pathFor(link.pathname);
    if (!path) return;
    event.preventDefault();
    void navigate(path, true);
  });
  window.addEventListener("popstate", () => {
    const path = pathFor(location.pathname);
    if (path) void navigate(path, false);
  });
  void views[activePath].load();
}
