import { loadFrontendModule } from "./frontend-module-loader.mjs";

class FakeClassList {
  constructor(owner) {
    this.owner = owner;
    this.tokens = new Set();
  }

  setFromString(value) {
    this.tokens = new Set(
      String(value || "")
        .split(/\s+/)
        .map((token) => token.trim())
        .filter(Boolean),
    );
  }

  add(...tokens) {
    for (const token of tokens) {
      if (token) this.tokens.add(token);
    }
  }

  remove(...tokens) {
    for (const token of tokens) {
      this.tokens.delete(token);
    }
  }

  toggle(token, force) {
    if (force === true) {
      this.tokens.add(token);
      return true;
    }
    if (force === false) {
      this.tokens.delete(token);
      return false;
    }
    if (this.tokens.has(token)) {
      this.tokens.delete(token);
      return false;
    }
    this.tokens.add(token);
    return true;
  }

  contains(token) {
    return this.tokens.has(token);
  }

  toString() {
    return [...this.tokens].join(" ");
  }
}

function makeStorage() {
  const data = new Map();
  return {
    getItem(key) {
      return data.has(key) ? data.get(key) : null;
    },
    setItem(key, value) {
      data.set(key, String(value));
    },
    removeItem(key) {
      data.delete(key);
    },
  };
}

function matchesSelector(element, selector) {
  if (!selector) return false;

  if (selector.startsWith(".")) {
    return element.classList.contains(selector.slice(1));
  }

  if (selector.startsWith("#")) {
    return element.id === selector.slice(1);
  }

  const dataMatch = selector.match(/^\[data-([a-z0-9-]+)(?:="([^"]*)")?\]$/i);
  if (dataMatch) {
    const key = dataMatch[1].replace(/-([a-z])/g, (_, part) => part.toUpperCase());
    if (!(key in element.dataset)) return false;
    return dataMatch[2] === undefined || element.dataset[key] === dataMatch[2];
  }

  return element.tagName.toLowerCase() === selector.toLowerCase();
}

function collectMatches(root, selector, acc) {
  for (const child of root.children) {
    if (matchesSelector(child, selector)) {
      acc.push(child);
    }
    collectMatches(child, selector, acc);
  }
}

export class FakeElement {
  constructor(tagName = "div", ownerDocument = null) {
    this.tagName = String(tagName).toUpperCase();
    this.ownerDocument = ownerDocument;
    this.children = [];
    this.parentNode = null;
    this.dataset = {};
    this.style = {};
    this.attributes = new Map();
    this.classList = new FakeClassList(this);
    this._className = "";
    this._textContent = "";
    this._innerHTML = "";
    this.value = "";
    this.onclick = null;
    this.disabled = false;
    this.tabIndex = 0;
    this.type = "";
    this.title = "";
    this.scrolledIntoView = null;
    this.stats = {
      innerHTMLWrites: 0,
      textWrites: 0,
      appendChildCalls: 0,
      removeCalls: 0,
    };
  }

  get className() {
    return this.classList.toString();
  }

  set className(value) {
    this.classList.setFromString(value);
  }

  get textContent() {
    if (this.children.length > 0) {
      return this.children.map((child) => child.textContent).join("");
    }
    return this._textContent;
  }

  set textContent(value) {
    this.stats.textWrites += 1;
    this.ownerDocument?.stats && (this.ownerDocument.stats.textWrites += 1);
    this._textContent = String(value ?? "");
    this._innerHTML = "";
    this.children = [];
  }

  get innerHTML() {
    if (this.children.length > 0 && this._innerHTML === "") {
      return this.children.map((child) => child.outerHTML).join("");
    }
    return this._innerHTML;
  }

  set innerHTML(value) {
    this.stats.innerHTMLWrites += 1;
    if (this.ownerDocument?.stats) {
      this.ownerDocument.stats.innerHTMLWrites += 1;
      this.ownerDocument.stats.clearedChildren += this.children.length;
    }
    this._innerHTML = String(value ?? "");
    this.children = [];
  }

  get id() {
    return this.attributes.get("id") || "";
  }

  set id(value) {
    this.attributes.set("id", String(value));
  }

  get outerHTML() {
    const attrs = [];
    if (this.id) attrs.push(`id="${this.id}"`);
    const className = this.className;
    if (className) attrs.push(`class="${className}"`);
    if (this.title) attrs.push(`title="${this.title}"`);
    for (const [key, value] of Object.entries(this.dataset)) {
      attrs.push(
        `data-${key.replace(/[A-Z]/g, (part) => `-${part.toLowerCase()}`)}="${String(value)}"`,
      );
    }
    const attrText = attrs.length > 0 ? ` ${attrs.join(" ")}` : "";
    return `<${this.tagName.toLowerCase()}${attrText}>${this.innerHTML || this.textContent}</${this.tagName.toLowerCase()}>`;
  }

  appendChild(child) {
    child.remove();
    child.parentNode = this;
    this.children.push(child);
    this._innerHTML = "";
    this.stats.appendChildCalls += 1;
    this.ownerDocument?.stats && (this.ownerDocument.stats.appendChildCalls += 1);
    return child;
  }

  append(...children) {
    for (const child of children) {
      if (child instanceof FakeElement) {
        this.appendChild(child);
      }
    }
  }

  insertBefore(child, referenceNode) {
    if (referenceNode === null || referenceNode === undefined) {
      return this.appendChild(child);
    }

    const existingIndex = this.children.indexOf(child);
    if (existingIndex >= 0) {
      this.children.splice(existingIndex, 1);
    } else {
      child.remove();
    }

    const nextIndex = this.children.indexOf(referenceNode);
    if (nextIndex < 0) {
      return this.appendChild(child);
    }

    child.parentNode = this;
    this.children.splice(nextIndex, 0, child);
    this._innerHTML = "";
    this.stats.appendChildCalls += 1;
    this.ownerDocument?.stats && (this.ownerDocument.stats.appendChildCalls += 1);
    return child;
  }

  removeChild(child) {
    const index = this.children.indexOf(child);
    if (index >= 0) {
      this.children.splice(index, 1);
      child.parentNode = null;
      this.stats.removeCalls += 1;
      this.ownerDocument?.stats && (this.ownerDocument.stats.removeCalls += 1);
    }
    return child;
  }

  remove() {
    if (this.parentNode) {
      this.parentNode.removeChild(this);
    }
  }

  replaceChildren(...children) {
    this.children = [];
    this._innerHTML = "";
    for (const child of children) {
      if (child instanceof FakeElement) {
        this.appendChild(child);
      }
    }
  }

  setAttribute(name, value) {
    this.attributes.set(String(name), String(value));
    if (name === "id") {
      this.id = value;
    }
  }

  getAttribute(name) {
    return this.attributes.get(String(name)) ?? null;
  }

  removeAttribute(name) {
    this.attributes.delete(String(name));
  }

  querySelector(selector) {
    return this.querySelectorAll(selector)[0] || null;
  }

  querySelectorAll(selector) {
    const matches = [];
    collectMatches(this, selector, matches);
    return matches;
  }

  closest(selector) {
    let current = this;
    while (current) {
      if (matchesSelector(current, selector)) return current;
      current = current.parentNode;
    }
    return null;
  }

  contains(node) {
    if (node === this) return true;
    return this.children.some((child) => child.contains(node));
  }

  focus() {}

  select() {}

  setSelectionRange() {}

  scrollIntoView(options = null) {
    this.scrolledIntoView = options;
  }
}

export class FakeDocument {
  constructor() {
    this.title = "";
    this.hidden = false;
    this.stats = {
      createElementCalls: 0,
      innerHTMLWrites: 0,
      textWrites: 0,
      appendChildCalls: 0,
      removeCalls: 0,
      clearedChildren: 0,
    };
    this.body = new FakeElement("body", this);
  }

  createElement(tagName) {
    this.stats.createElementCalls += 1;
    return new FakeElement(tagName, this);
  }

  createTextNode(text) {
    const node = new FakeElement("#text", this);
    node.textContent = text;
    return node;
  }

  getElementById(id) {
    return this.body.querySelector(`#${id}`);
  }

  querySelector(selector) {
    return this.body.querySelector(selector);
  }

  querySelectorAll(selector) {
    return this.body.querySelectorAll(selector);
  }

  execCommand() {
    return true;
  }

  addEventListener() {}

  removeEventListener() {}
}

export function installFakeDom() {
  const documentStub = new FakeDocument();
  const windowStub = {
    __RESTREAM_BASE_PATH__: "",
    location: {
      href: "http://localhost/",
    },
    history: {
      pushState() {},
      replaceState() {},
    },
    localStorage: makeStorage(),
    sessionStorage: makeStorage(),
    addPipeBtn() {},
    addEventListener() {},
    removeEventListener() {},
  };

  Object.defineProperty(globalThis, "document", {
    value: documentStub,
    configurable: true,
  });
  Object.defineProperty(globalThis, "window", {
    value: windowStub,
    configurable: true,
  });
  Object.defineProperty(globalThis, "navigator", {
    value: {
      clipboard: {
        async writeText() {},
      },
    },
    configurable: true,
  });
  Object.defineProperty(globalThis, "HTMLElement", {
    value: FakeElement,
    configurable: true,
  });
  Object.defineProperty(globalThis, "HTMLButtonElement", {
    value: FakeElement,
    configurable: true,
  });
  Object.defineProperty(globalThis, "HTMLInputElement", {
    value: FakeElement,
    configurable: true,
  });
  Object.defineProperty(globalThis, "HTMLDialogElement", {
    value: FakeElement,
    configurable: true,
  });
  Object.defineProperty(globalThis, "CSS", {
    value: {
      escape(value) {
        return String(value);
      },
    },
    configurable: true,
  });
  Object.defineProperty(globalThis, "__FRONTEND_MODULE_TOKEN__", {
    value: `${Date.now()}-${Math.random().toString(16).slice(2)}`,
    configurable: true,
    writable: true,
  });

  return { document: documentStub, window: windowStub };
}

export async function loadCompiledFrontendModule(relativePath) {
  return loadFrontendModule(relativePath);
}
