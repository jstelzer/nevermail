mod app;
mod dnd_models;
mod ui;

fn main() -> cosmic::iced::Result {
    env_logger::init();

    let settings = cosmic::app::Settings::default()
        .size_limits(
            cosmic::iced::Limits::NONE
                .min_width(800.0)
                .min_height(400.0),
        );

    cosmic::app::run::<app::AppModel>(settings, ())
}
