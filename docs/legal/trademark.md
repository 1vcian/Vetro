# Trademark note: "Vetro"

> **Internal draft** (product track B1), not legal advice. Stays in the
> private repository (`tools/export/allow.txt`). Everything here must be
> checked with a trademark attorney (in Italy: a *consulente in proprietà
> industriale* registered with the Ordine) before filing or spending money.

## Why register

The public repository will be open source: anyone may fork the code, but not
the name. A registered mark lets the owner stop forks and hosted services from
calling themselves "Vetro", backs the NOTICE line that the licence does not
cover the name and logo, and is expected by app stores, payment providers and
enterprise customers. Without a registration the owner only has weak,
country-by-country rights from use.

## What to register

1. **Word mark "VETRO"** — the core asset. It protects the name in any font.
2. **Figurative mark: the logo** (`docs/assets/vetro-icon.png`), after the
   design is final. Cheaper to add later; worth it once the logo is on the
   landing page.
3. Maybe a combined mark (logo + word) instead of 2, if the attorney
   suggests it for cost reasons.

Classes (Nice classification) and wording to discuss:
- **Class 9**: downloadable software; software for emulating computer
  systems and mobile operating systems; software for the security and
  privacy analysis of applications.
- **Class 42**: software as a service (SaaS) and platform as a service;
  providing online non-downloadable software for emulation, application
  testing and security analysis; computer security consultancy.
- Optional, **class 41**: training and education (if courses or workshops
  are planned).

Each class costs extra, so file only what the business will really use in
the next five years (a mark can be revoked for non-use after five years).

## Where

| Office | Covers | Notes (fees indicative, check current ones) |
|---|---|---|
| **EUIPO** (EU trade mark) | all 27 EU member states, Italy included | first choice; online filing about €850 for one class, €50 for the second, €150 for each further class; 10 years, renewable |
| UIBM (Italy) | Italy only | cheaper fallback if an EU-wide conflict blocks the EU mark |
| USPTO (United States), UKIPO (UK), Switzerland | those countries | via the **Madrid Protocol** (WIPO), based on the EU application, within the 6-month priority period if the US/UK market matters at launch (B5) |

Filing the EU application first gives **6 months of priority** (Paris
Convention) to extend to other countries with the same filing date.

## Search before filing (the attorney does the full one)

- **Identical and similar marks** in classes 9 and 42: EUIPO eSearch plus,
  TMview (EU and national offices), WIPO Global Brand Database, USPTO search.
  "Vetro" is an Italian word ("glass"), so it is common in names of glass,
  optics and design businesses — mostly other classes, which is fine — but
  there are software uses too: for example **VETRO FiberMap**, a US fibre
  network planning software company, uses VETRO for software/SaaS. Whether
  that blocks a US filing, or only limits the description of services, is
  the first question for the attorney.
- **Unregistered use**: GitHub projects, app stores, company registers,
  domains named Vetro in software.
- **Descriptiveness**: "glass" does not describe an emulator, so the word is
  distinctive for software; the attorney confirms for the Italian public,
  where the word is ordinary language.

If the search shows a real conflict, decide early whether to keep "Vetro"
with a distinguishing element (for example a combined mark) or to rename
before B5: renaming after launch costs much more.

## Around the registration

- **Domains**: `vetro.lol` is being bought; consider defensive domains
  (`.app`, `.dev`) only if cheap.
- **Trademark policy for the community** (public repository, B3): forks must
  change the name and logo; describing compatibility ("works with Vetro",
  "based on Vetro") is allowed; the official builds are those from
  `vetro.lol`. Short text, in the style of other open source projects.
- **Third-party marks**: the product never presents itself as "Android"
  (CLAUDE.md); follow Google's brand guidelines for descriptive references,
  keep the "not affiliated with Google" line in the README and on the site,
  and keep ADR 0030's overlays that remove Google marks from the image.
- **Use the ™ symbol** until registration, ® only after it.

## Checklist for the owner

1. Ask a *consulente in proprietà industriale* for a clearance search
   (VETRO, classes 9 and 42, EU and US).
2. Decide word mark only, or word + logo; decide classes.
3. File at EUIPO; note the priority deadline (+6 months) for Madrid
   extensions.
4. Put the trademark policy in the public repository at B3.
