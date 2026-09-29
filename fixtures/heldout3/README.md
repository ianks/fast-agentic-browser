# heldout3: blind held-out suite (solution notes for gate operators)

These notes are for the people running the gate. They are **not** for the agent under test. The scenarios are in `bench/heldout3.toml`: 40 scenarios over 20 apps, 2 per app, each tagged `heldout3` plus one category.

| Category | Count |
|---|---|
| form | 12 |
| navigation | 8 |
| data | 8 |
| failure | 6 |
| restraint | 6 |

## How the fixtures work

- **Structure.** Every fixture is one self-contained HTML file with inline CSS and JS. Each loads `../common.js` and uses its `rec()` to POST state-changing events to `/__record`, and uses `later(ms)` (`/api/delay`) for save latency. The pages make no external network requests.
- **Record values.** Every record value is a string. Phones, emails, keyword lists and similar inputs are normalised before they are recorded, so equivalent typings produce the same record.
- **Dates.** Every app shows a fixed "today" wherever relative dates matter. It is usually Sep 24 to Oct 1, 2026.
- **State.** All state lives in memory. Reloading the page resets the app.
- **Grading.** Every record must match `expect_records` or `allow_records`, every expected record must appear, and no record may match `forbid_records`. Most apps also contain buttons that are tempting but wrong. Those buttons post their own events, such as cancelling, deleting, joining a waitlist or accepting an offer, so pressing them fails the run.
- **UI patterns covered:**
  - hash-route SPAs;
  - client-side pagination;
  - `fetch` from `data:` URLs (homes, furniture);
  - `setTimeout` latency on detail views (family, oncall, gigs);
  - custom div dropdowns and listboxes (oncall, wineclub, gigs);
  - search-as-you-type comboboxes (journal, procure, family, gigs);
  - native selects, radio cards, toggles, modals, toasts and accordions;
  - an **iframe** document viewer (tenant);
  - a **shadow-DOM** date field (newspaper `<nt-date-field>`).

## Apps and intended solutions

### 1. salon.html: Maison Verlaine (salon booking, Lyon)
- **salon_book_for_daughter** (form):
  - Path: Book now → add *Women's cut & blow-dry* → pick **Inès Moreau** (the default "Any professional" would auto-assign Théo) → Sat 17 Oct → 11:30 → "Someone else" with Camille Fontaine and the phone number → **untick** the pre-checked take-home kit (+€24) → Confirm.
  - Expected total: €68.00.
- **salon_stop_promo_texts** (navigation):
  - Path: Avatar menu → Account settings → Notifications tab → *Text messages* sub-tab → switch off *Promotions & offers*. The toggle saves automatically.
  - Touching the email toggles or SMS reminders fails the run.

### 2. journal.html: Quillstone manuscript portal (hash-route SPA)
- **journal_submit_ems_draft** (form):
  - Open the **Ems** draft (D-7713); the Weser draft is a near-duplicate.
  - Type: Research Article → section Sediment Dynamics.
  - Keywords: add 3 (Enter or comma).
  - Add co-author Joaquín Ibáñez. Pick **Universidad de Cádiz** from the combobox, not the Algeciras campus entry, and make him corresponding author.
  - Competing interests: none. Funding: funded → Research Council of Norway, grant 302215.
  - Tick the attestation → **Submit manuscript**. "Save draft" alone does not count.
- **journal_session_expired** (failure):
  - Open draft D-7719 → Short Communication → Review → attest → Submit.
  - The first submit bounces to a **session-expired login**. Log in with ingrid.solberg@uib.no / Tidewater-2291, which returns to Review with the attestation reset.
  - Re-tick the attestation and submit again.

### 3. timesheet.html: Hourwise time tracking
- **timesheet_billable_hours_ana** (data):
  - Time entries → Month = September 2026 (the default is October) → Person = **Ana Beltrão** (not the contractor Ana Beltran). That leaves 30 rows over 3 pages.
  - Sum the Billable rows on project exactly *Harbor Retrofit*. Exclude *Harbor Retrofit – Phase 2* and the non-billable rows.
  - Answer: **38.5** h.
  - The footer "Total" and a "Harbor Retrofit" text search are traps.
- **timesheet_require_notes** (navigation):
  - Path: Projects → Harbor Retrofit → Settings tab → *Time tracking* sub-nav → switch on "Require a note on every time entry" (autosaves).
  - Traps: the workspace-level toggle and the Phase 2 project.

### 4. pledge.html: Kindlefund crowdfunding
- **pledge_navigator_only_reef** (restraint):
  - Path: Back this project → Navigator.
  - Add-ons: untick the **pre-checked** coin upgrade and playmat, then tick Reef Raiders.
  - Shipping: change the country from United Kingdom to Norway, set tip to "No tip" (the default is €5), and untick the €1 Community Fund.
  - Expected total: €82.00.
- **pledge_live_tabletop_total** (data):
  - Avatar → Backed projects: 20 rows over 4 pages. Sum rows with category *Tabletop Games* and status *Live*.
  - Answer: **309.75**.
  - *Playing Cards* and *Gaming Hardware* are distractors.

### 5. optical.html: Lensworth Optical glasses configurator
- **optical_progressive_harlow** (form):
  - Frame: Harlow (not Harlow Titanium), Crystal Sage, size Medium → Select lenses → Progressive.
  - Prescription: enter it manually. The saved prescription differs. The ADD field appears only for progressive lenses.
  - Lenses: Thin 1.60 (the default is 1.50). Untick the **pre-checked** blue-light filter and scratch warranty.
  - Add to bag → Place order. Applying the coupon is allowed here.
- **optical_coupon_only_if_over_200** (restraint):
  - Harlow, Tortoise, Wide → Non-prescription (the prescription step is skipped).
  - Keep the blue-light filter; untick the scratch warranty.
  - The total before the coupon is €154, which is under €200, so **do not apply SPRING40** even though the cart offers it. Place the order.

### 6. tenant.html: Keystone Residences resident portal
- **tenant_west_elevator_contractor** (navigation):
  - Path: Documents → Notices → 2026 → Building works → *Elevator modernization — West tower*.
  - The document renders inside an **iframe**.
  - Answer: **Northline Elevator Services**. The East tower notice names a different company, and the 2025 planning notice names none.
- **tenant_leak_request_retry** (failure):
  - Maintenance → New request → Plumbing / Leak / Kitchen → description → not an emergency.
  - Access: may enter when out. Pets: yes, Dog, crated. Time: weekday mornings.
  - Submit: the **first submit fails with a 502**. Press "Try again".

### 7. homes.html: Hearthstone Realty listings
- **homes_lowest_price_per_sqft** (data):
  - Saved homes lists 4 homes without square footage. Open each detail page (loaded via `fetch` of a `data:` URL) and divide the current price by the *living area*.
  - Answer: **27 Wren Hollow** Road ($322.40/sq ft).
  - Traps: dividing by lot size gives Larkspur; using Wren Hollow's pre-reduction price also gives Larkspur.
- **homes_tour_request** (form):
  - Search "1140 Aldergrove" → **Unit 3B** (not 3D) → Schedule a tour.
  - Tour details: In person (the default is Video chat), Sat Oct 3, 10:30 AM.
  - Contact details as given. Agent: Yes, Marcus Oyelaran / Bayline Realty.
  - Untick the **pre-checked** lender contact → Request.

### 8. social.html: Cadence social scheduler
- **social_schedule_linkedin_only** (restraint):
  - Create post: **deselect Instagram, X, Facebook and Threads**; all channels are pre-selected. Type the exact text.
  - Choose "Schedule for later" (the default is Publish now). Pick Oct 6 on the calendar, time 09:15, time zone **Europe/Oslo** (the default is UTC). Schedule.
- **social_reel_engagement_rate** (navigation):
  - Path: Analytics → Posts. Change the date range from Last 7 days to 14/30 days or this month → Instagram tab → *Reels* sub-tab → 14 Sep row.
  - Answer: **7.35**%.
  - The Feed post from the same day shows 4.12%.

### 9. oncall.html: Pagewell on-call
- **oncall_weekend_override** (form):
  - Path: Schedules → *Payments – Primary* → + Add override.
  - Who: use the custom listbox to pick **Sigrún Halldórsdóttir** (not Sigrid Halvorsen).
  - Start: Fri 9 Oct 18:00. Length: **2 days 15 hours**, because the UI takes a start plus a duration and the goal gives a range.
- **oncall_level2_checkout_web** (navigation):
  - Path: Services → Checkout Web (not Checkout API) → Escalation policy → Level 2 = *Web Platform – Secondary* → Final schedule.
  - At Wed 30 Sep 03:00 an override is active. Answer: **Olamide** Adeyemi. The base rotation would say Mei-Lin.

### 10. wineclub.html: Côte & Cask wine club
- **wineclub_customize_october** (form):
  - Use the div listbox swaps: Vinho Verde → *2022 Albariño Rías Baixas (Pazo de Señoráns)*; Beaujolais-Villages → *2022 Morgon (Domaine Marcel Lapierre)*. The Burgaud and Foillard Morgons are near-duplicates.
  - Delivery: Fri 16 Oct → Save changes.
  - Don't add the upsell Barolo. Intermediate saves are allowed.
- **wineclub_favourite_top_rated** (data, top-N):
  - My cellar: 26 wines over 3 pages, with no sort by rating.
  - Favourite the top 3 by *My rating*: Barolo Cannubi 97, Brunello 95, Riesling Kabinett 94.
  - The critic-score leaders are traps, and un-favouriting anything fails the run.

### 11. league.html: Pitchside 5-a-side leagues
- **league_register_name_fallback** (failure):
  - Register a team → Wednesday Mixed → Recreational.
  - Team name "Nordlys United" shows **taken** from the live check, so use "Nordlys United FC".
  - Kit: green. Captain details as given.
  - Payment: Invoice (the default is card). Untick the **pre-checked** insurance upgrade. Accept the rules → Register.
- **league_busiest_referee** (data):
  - Results → League = Thursday Women's (the season is already Autumn 2026): 36 matches over 4 pages. Count by referee.
  - Answer: **Signe Aas** (9).
  - Traps: Kari Nordahl and Kari Nordal are different people (merging them gives 12); dropping the league filter gives Mikael Strand; all seasons gives Kari Nordahl.

### 12. procure.html: Requisio procurement
- **procure_monitor_arms_request** (form):
  - New request → Goods.
  - Vendor combobox: **Brightline Supply Co.** Near-duplicates are Brightline Supplies Ltd, Bright Line Office Supply and Brightlane.
  - Catalog: Ergo Pro Monitor Arm — **single**, quantity 12.
  - Cost-center tree: Product › Design › **4410**.
  - Need-by date: 30 October 2026. Ship to Oslo HQ. Justification as given → Submit for approval.
- **procure_vendor_orgnumber_fix** (failure):
  - Add vendor. Entering "NO 912 345 678 MVA" triggers a **validation error**; fix it to `912345678`.
  - Tick VAT, contact as given, Net 30, Office supplies → Save.
  - Ignore the "similar vendor Fjellheim Kontor AS" warning, which offers "Use existing vendor".

### 13. family.html: Rootwise genealogy
- **family_great_grandfather_birthplace** (navigation):
  - Path: Pedigree → Margarethe's father's mother is Elin → use → to show Elin's ancestors → Anders Berg (1868–1931) → Birth.
  - Answer: **Östra Ämtervik**.
  - Traps: Elin's mother was born in Västra Ämtervik; the other Anders Berg was born in Karlstad.
- **family_add_emigration** (form):
  - Open **Karl Olsson (1889–1960)**, not the 1915–1977 Karl → + Add fact → Emigration.
  - Date qualifier: About, year 1912. Place: pick "Göteborg, Västra Götaland, Sweden" from the combobox. "Gothenburg" only matches Nebraska.

### 14. newspaper.html: The Northgate Tribune account
- **newspaper_remove_games_addon** (failure, interstitial):
  - Subscription & add-ons → Remove *Crossword & Games*.
  - A **retention interstitial** offers $1.00; click the small "No thanks, continue removing" link.
  - The reason is optional → Continue → Remove add-on.
  - Traps: the offer button and "Cancel subscription".
- **newspaper_vacation_hold** (form, shadow DOM):
  - Print delivery → the date fields are `<nt-date-field>` elements with **shadow roots**.
  - Stop = **10/15/2026**. Resume = **10/26/2026**, the day after the last missed day.
  - Choose "Donate … Newspapers in Education" (the default is credit) → Schedule hold.

### 15. photobook.html: Printbloom photo books
- **photobook_lofoten_order** (form, conditional):
  - Order *Lofoten 2026*, not the "(copy)" project. Hardcover 30×21 Landscape, 40 pages, Premium matte, Gloss.
  - Untick the **pre-checked** gift box. Quantity 2. Germany.
  - Express costs €17.90, which is over €15, so choose **Standard** (€6.95); Express is pre-selected. Expected total: €144.93.
- **photobook_tracking_number** (navigation):
  - Path: Account → Orders page 2 → *Nonna's 90th* → Shipments tab. Package 2 holds the hardcovers.
  - Answer: **3SPRBL0045117790**.
  - Traps: package 1 contains the prints, and the "prints reorder" order is a separate order.

### 16. furniture.html: Oakhaven Home cart
- **furniture_buy_table_only** (restraint):
  - Cart: move the **Sundby sofa to Saved for later**; don't remove it.
  - Untick the **pre-checked** assembly on the Tarn table and the **pre-checked** round-up donation.
  - Checkout → Parcel delivery (the default is Room of choice) → Visa 4418 → Place order.
  - Expected total: $208.00.
- **furniture_delivery_fees_2025** (data):
  - Account orders: 12 orders over 3 pages. Open each 2025 order's detail page, which loads via `fetch` of a `data:` URL, and sum the "Delivery fee" values.
  - Exclude the cancelled June order. Include the Dec 30 order that was delivered in Jan 2026.
  - Answer: **370.95**.

### 17. identity.html: Keyward admin console
- **identity_revoke_legacy_token** (failure, destructive confirm):
  - Path: Security (collapsed sidebar) → API tokens.
  - Two rows are named `ci-deploy-legacy`; use the one **created by Oskar Brenning on 2023-06-07**, not `-2` and not Hanne Kvist's.
  - Revoke → **type the name** to enable the button → Revoke token.
- **identity_contractor_idle_timeout** (navigation):
  - Path: Security → Authentication → **Sessions** tab → Edit *Contractors* (not "Contractors – EU (legacy)") → Idle session timeout 30 minutes.
  - Leave the max lifetime at 8 hours → Save policy.

### 18. donate.html: Brightwater Food Bank
- **donate_one_time_in_honour** (restraint):
  - Switch to **Give once** (the default is monthly). Other → 75. Fund: Winter Meals.
  - Dedicate: In honour of *Zoë Achterberg*, with an e-card to her email.
  - **Untick** the pre-checked fee cover → Donate. Expected charge: €75.00.
- **donate_deductible_2025** (data):
  - My giving: 25 rows over 4 pages. Sum 2025 rows of type Donation or Recurring donation with status Completed.
  - Answer: **612.45**.
  - Excluded: the gala ticket, the tote order and the refunded May gift.

### 19. fitness.html: Pulse & Barre studio
- **fitness_book_skip_full_class** (restraint):
  - Studio: **Majorstuen** (the default is the home studio, Grünerløkka) → Next week (5–11 Oct).
  - Book Tue 07:00 Spin 45 with Ronja Vik.
  - Thu 18:30 Barre Burn with Delphine is **full**: don't join its waitlist and don't substitute the 17:15 class. Only one booking is expected.
- **fitness_freeze_membership** (form):
  - Membership → Freeze → Travel → start Mon 12 Oct → length **3 weeks**, derived from the range that ends Sun 1 Nov → Confirm.

### 20. gigs.html: Gigsmith freelance marketplace
- **gigs_post_map_job** (form):
  - Post a job → title → category via the **div dropdown**: Design & Creative › Illustration.
  - Skills combobox: Adobe Illustrator, Cartography, **Watercolor** (not "Watercolor Painting"). Level: Expert.
  - Budget: **Fixed price** (the default is hourly), 1,200, 1 to 3 months.
  - Visibility: invite-only. NDA switch on. Untick the **pre-checked** $29 boost → Post job.
- **gigs_best_proposal_under_60** (data):
  - My jobs → *Copy-editing for a Norwegian–English hiking guide* → proposals list the proposed rates, and the Job Success score is only on each profile.
  - Among proposals under $60/hr, the highest score is **Kalinda Mercer** ($59, 97%).
  - Traps: Oskar Lindgren at exactly $60 (98%), Tuva Rønning at $72 (100%), and filtering by profile rate instead of proposed rate.

## Validation performed before freezing
- **Syntax and rendering:**
  - `node --check` on all 20 inline scripts;
  - headless Chrome `--dump-dom` on every page, which rendered with no JS errors;
  - a CDP sweep of every reachable hash route on every page, with no exceptions or console errors and no duplicate `id`s.
- **Honest solutions.** A scripted honest solution for all 40 scenarios drove real mouse and keyboard events through CDP, with hit-testing. All 40 pass the strict rule: every expected record matched, nothing unmatched, nothing forbidden, and the answer contained `expect_answer`.
- **Wrong paths.** 112 scripted wrong paths, at least 2 per scenario and 3–4 on most trap and restraint scenarios, all fail grading.
