/* API reference enhancements. Turns the plain "Endpoint" list into a method
   badge plus path, and tags status headings and Required cells so the
   stylesheet can color them with the Membrick palette. */

function enhanceApiReference() {
  document.body.classList.toggle("api-page", /\/api\//.test(location.pathname));
  const article = document.querySelector(".md-content article");
  if (!article) return;

  const heading = article.querySelector("h2#endpoint");
  if (heading && !heading.dataset.apiEnhanced) {
    heading.dataset.apiEnhanced = "true";
    const list = heading.nextElementSibling;
    if (list && list.tagName === "UL") {
      let method = "";
      let url = "";
      const consumed = [];
      list.querySelectorAll("li").forEach((item) => {
        const code = item.querySelector("code");
        const text = item.textContent.trim();
        if (code && /^Method:/.test(text)) {
          method = code.textContent.trim();
          consumed.push(item);
        } else if (code && /^URL:/.test(text)) {
          url = code.textContent.trim();
          consumed.push(item);
        }
      });
      if (method && url) {
        const bar = document.createElement("div");
        bar.className = "api-endpoint api-endpoint--" + method.toLowerCase();
        const badge = document.createElement("span");
        badge.className = "api-method api-method--" + method.toLowerCase();
        badge.textContent = method;
        const path = document.createElement("code");
        path.className = "api-path";
        path.textContent = url;
        bar.append(badge, path);
        heading.after(bar);
        consumed.forEach((item) => item.remove());
        if (!list.querySelector("li")) list.remove();
      }
    }
  }

  article.querySelectorAll("h3").forEach((h3) => {
    const text = h3.textContent.replace("¶", "").trim();
    if (/^2\d\d$/.test(text)) h3.classList.add("api-status", "api-status--ok");
    else if (/^[45]\d\d$/.test(text)) h3.classList.add("api-status", "api-status--error");
    else if (text === "Errors") h3.classList.add("api-status", "api-status--error");
  });

  article.querySelectorAll("table").forEach((table) => {
    const headers = Array.from(table.querySelectorAll("thead th")).map((th) => th.textContent.trim());
    const col = headers.indexOf("Required");
    if (col === -1) return;
    table.querySelectorAll("tbody tr").forEach((row) => {
      const cell = row.children[col];
      if (!cell) return;
      const value = cell.textContent.trim();
      if (value === "Yes") cell.classList.add("api-required");
      else if (value.startsWith("No")) cell.classList.add("api-optional");
    });
  });
}

if (typeof document$ !== "undefined") {
  document$.subscribe(enhanceApiReference);
} else {
  document.addEventListener("DOMContentLoaded", enhanceApiReference);
}
