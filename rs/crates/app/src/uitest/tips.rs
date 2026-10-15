//! Agent tips: the ✦ rail button opens the TIPS section, Archive moves a tip out and the
//! archive view offers Restore. Both callbacks must carry the id of the row clicked.
#![allow(unused_imports)]

use super::*;
use std::cell::RefCell;
use std::rc::Rc;

fn row(id: i32, text: &str) -> crate::TipRowUi {
    crate::TipRowUi {
        id,
        title: "builder".into(),
        text: text.into(),
        context: "editing parser".into(),
        age: "just now".into(),
        reason: "".into(),
    }
}

#[test]
fn archive_and_restore_reach_rust_with_the_rows_id() {
    ui(|| {
        let w = window();
        let g = w.global::<crate::TipsAdapter>();
        g.set_rows(Rc::new(slint::VecModel::from(vec![row(7, "use ctrl+o")])).into());
        g.set_archived(Rc::new(slint::VecModel::from(vec![row(3, "old tip")])).into());

        let archived = Rc::new(RefCell::new(Vec::new()));
        let restored = Rc::new(RefCell::new(Vec::new()));
        let a = archived.clone();
        g.on_archive(move |id| a.borrow_mut().push(id));
        let r = restored.clone();
        g.on_restore(move |id| r.borrow_mut().push(id));

        let open = by_label(&w, "Agent tips — harvested from your agents");
        assert_eq!(open.len(), 1, "the rail offers one tips button");
        click(&w, &open[0]);
        assert!(g.get_open(), "the button opens the section");

        let arch = by_label(&w, "Move to the archive — nothing is deleted");
        assert_eq!(arch.len(), 1, "one Archive button per active tip");
        click(&w, &arch[0]);
        assert_eq!(*archived.borrow(), vec![7]);

        let view = by_label(&w, "Switch between the active tips and the archive");
        assert_eq!(view.len(), 1);
        click(&w, &view[0]);
        assert!(g.get_show_archive(), "the archive view opens");

        let back = by_label(&w, "Move back to the active tips");
        assert_eq!(back.len(), 1, "one Restore button per archived tip");
        click(&w, &back[0]);
        assert_eq!(*restored.borrow(), vec![3]);
    });
}
