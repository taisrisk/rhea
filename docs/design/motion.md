# Motion design

The app is being brought in line with these 20 guidelines; the table says which are applied today. All timings live in `src/app/motion.rs`.

| # | Guideline | Where it is applied |
| --- | --- | --- |
| 1 | Ease-out for entrances | Every animated value uses cubic-out: hover, panel switches, menus, switches |
| 2 | Dropdowns at 150–250 ms | Hover 150 ms; panel switches and the switch knob 220 ms; popups |
| 3 | Stagger list rows by 30 ms | History rows enter one after another when the panel appears |
| 4 | Scale to 0.97 on press | Buttons, icon buttons, menu items and the send button |
| 5 | Stretch moving knobs 12% | The AutoCorrect switch knob while it travels |
| 6 | Stagger as details settle | History rows follow one another; their text does not stagger separately yet |
| 7 | Arcs for object movement | The activity ring turns along a circular arc |
| 8 | Pulse to confirm success | Not applied yet |
| 9 | Ease-in-out for moves | The tab pill sliding between History and Subagents |
| 10 | Start popovers at 0.95 | Not applied yet: menus use egui's own fade-in |
| 11 | Enter from the trigger | Not applied yet: menus open beside their button |
| 12 | Exit to the trigger | Not applied yet |
| 13 | No bounce on everyday UI | No overshoot easing anywhere |
| 14 | Don't tween while held | A pressed control stays at 0.97 without re-animating |
| 15 | Exits 20% shorter | Press-in is 80 ms against 160 ms for release |
| 16 | Overlap old and new states | The History ⇄ Subagents switch fades and slides the new panel in |
| 17 | Masks for reveals and colour | Colour changes ease (delete turns red, buttons brighten); progress bars are plain fills |
| 18 | Crossfade icons | The send button's ring crossfades into the stop square on hover |
| 19 | Rubber band past bounds | Not applied: egui scroll areas stop at their bounds |
| 20 | Reuse the same layoutId | Elements that persist across states keep their id, so they move instead of jumping (the context window number, ring and bar ease to the new model's size) |

The send button's ring turns slowly (about one turn in three seconds) while only the main agent works and speeds up with every running subagent, easing between speeds.
