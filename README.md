# Dom

Smarthome app.

Robust. Efficient. Old school.

# Usage

A Terminal User Interface is available. Simply run the app.

You can exit the app at any time by hitting 'q'.

You can exit any popup or submenu by hitting the Escape key. Hitting Escape in the main screen will
also exit the app.

## Theme

You can switch between the dark and light theme with 'c'. The app will try to auto-detect the theme,
but for some terminal and multiplexer combinations there isn't enough information available to make
the correct choice.

## Main screen

The main UI screen is an overview of your home.

The systems are split into 5 categories:

- Energy: Power usage, power production, etc.
- Security: Alarm systems, cameras, smoke detectors, etc.
- Environment: Temperature, humidity, air quality, etc.
- Communication: LAN, WLAN, Meshtastic etc.
- Household: Robot vacuums, laundry, calendar, etc.

The main UI screen gives a summary of each category and a list of most important information for
this moment:

- Energy: Total consumption, production, storage and import gauge
- Security: Alarm state (Enabled/Disabled etc.) and "All clear" or "ALERT!" states
- Environment: A temperature, humidity and air quality gauges
- Communication: "Operational" if everything is ok, or "Issue detected" if not
- Household: Todays calendar events, last automated cleaning for each robot

Using 'e', 's', 'v' and 'h' you can navigate to detailed views for Energy, Security, Environment and
Household respectively. 'm' will take you back to the main view from any detailed view.

In each view, 'c' will take you to a configuration panel. In the main screen, you can configure
application wide settings. In detailed views, you can configure settings for each subsystem.

### Keyboard shortcuts

| Key | Action |
|-----|--------|
| q | Quit the app |
| Esc | Close popup or exit to previous screen (or quit if on main screen) |
| t | Toggle theme (dark/light) |
| c | Communication view |
| g | Configuration panel |
| a | Start device discovery |
| e | Energy view |
| s | Security view |
| v | Environment view |
| h | Household view |
| m | Main screen |

## Energy view

On top is the summary gauge of total energy production, consumption, storage and import.

Followed by itemized lists for each:

- Consumption
- Production
- Storage
- Import

Each list has a list of entities and their individual gauges. E.g. if you have multiple batteries,
each will be listed separately under storage. Or if you import both electricity and natural gas,
both will be listed under import. For production, you could have either multiple solar panel arrays,
wind turbines, hydro or a mix of them.

For consumption, production and import, each list row has 3 items:

1. The last observed value, in kW
2. The daily total for this day, in kWh
3. A histogram of observed values for the last 4 hours, auto-scaled to fit the rest of the screen.

For storage, each list row has 3 items:

1. The current input or output value, in kW
2. The current stored energy, in kWh
3. A histogram of observed charge/discharge values, for the last 4 hours, auto-scaled to fit the
   remaining screen width

### Keyboard shortcuts

Pressing '+' will bring up a popup allowing you to configure a new energy sensor. Note that
Dom tries to auto-detect as many sensors and devices as possible, so before completely manually
adding one, try the auto-detect feature first (using 'a' from any screen).

Pressing 'c' will open up the energy configuration panel.

## Security view

At the top of the screen is the summary of the current security system mode and a summary of the
current state. The state can be "All clear", "Warning" or "**ALERT**" depending on if any security
sensors are reporting any events.

The rest of the screen is a list of security sensors, grouped by sensor types. For each sensor, the
last time the sensor information was updated is displayed.

### Keyboard shortcuts

Pressing '+' will bring up a popup allowing you to configure a new security sensor. Note that
Dom tries to auto-detect as many sensors and devices as possible, so before completely manually
adding one, try the auto-detect feature first (using 'a' from any screen).

Pressing 'c' will open up the security configuration panel.

## Environment view

A simple list of sensors, with the reported temperature, humidity, air quality and any other extra
information is displayed. At the end of each row is the timestamp of the last time the sensor
information was updated.

## Communication view

Lists the routers, modems, access points and software defined switches and their general status,
ping latency, last bandwidth test results, including bandwidth in mbps and lost packets.

## Household view

A list of household automation objects, e.g. robot vacuums, robot lawnmovers, followed by the last
time the object was connected to and then a plain-text
status that each object reports.

# Installation

Use cargo install.

There are no packages currently available.

## Headless Mode

Dom can also run in headless mode for server/automation use:

```bash
dom --headless
# or
dom -h
```

In headless mode, Dom:
- Connects to all configured devices
- Collects sensor readings periodically
- Stores data in the time series database
- Runs controllers to make decisions

Configuration is read from the SQLite database (config table).

## Platform Support

Only Linux is supported. Support for macOS and Windows is a non-goal.

# Contact

You can contact me at [marko@ivankovic.me](marko@ivankovic.me).

# License

Copyright (C) 2026 Marko Ivankovic

Licensed under the [Prosperity Public License 3.0.0](https://prosperitylicense.com/versions/3.0.0).

- **Noncommercial use is free.** Personal use, hobby projects, study, research and
  experiment are all unrestricted. So is use by charities, educational institutions,
  public research bodies, public safety and health organizations, environmental
  protection organizations and government institutions — regardless of how they are
  funded.
- **Commercial use gets a thirty-day trial.** One trial per company, covering all
  personnel, not one trial per person. Past that, you need a license.
- **Contributions back don't count as commercial use.** Feedback, changes and additions
  contributed back under a standard permissive license (Blue Oak, Apache-2.0, MIT,
  BSD-2-Clause) are free to develop.

See the [LICENSE](LICENSE) file for the full terms.

Note that this is deliberately **not** an open source license: it does not meet the OSI
definition, and it is not free software in the FSF sense. That is the intent.

## Need a commercial license?

Commercial licensing is available, for individually negotiated compensation.

[Contact me](mailto:marko@ivankovic.me) for options.

## Previously AGPL-3.0

Up until August 2026, this project was published under AGPL-3.0 from a repository hosted
on Codeberg. That grant is irrevocable for the versions it was made under — anyone who
obtained the code under AGPL-3.0 keeps their AGPL-3.0 rights to those versions. The
Prosperity license applies to this repository and everything going forward.

# For Developers, human or otherwise

This part of the README is mostly used to tell the AI how to work in this code. Still, useful for humans too.

## Time and the real world

Across Dom, all timestamps, instants, durations and other time types **must** use milliseconds. The
only exception is in the very last moment before a value is displayed to the user. For usability,
a humanized text can be displayed. But such humanized values should never be stored or computed
with. Computations and storage **must** be done using 64bit types that represent milliseconds.

This is because milliseconds are a good middle ground between seconds, which would be too coarse for
things like light-switch operations and nanoseconds, which are far too granular for control that
goes over a network layer that has millisecond latency.

## Digital model of the real world

Dom has a digital model of your house.

The fundamental design principle for Dom is to accept that the digital model will always be a little
off of the real world. Sensors have latency, and we don't have sensors in every atom of every room.

The most important decision is to timestamp every event and use the timestamps to estimate the drift
between realiy and the model.

## Object oriented model, separation of concerns

Dom has the following design:

1.  The World Model - The digital twin of the world. It represents what Dom _believes_ the real
    world is. It also knows what the real world _should be_.
2.  The Device Manager - Discovers, configures, manages the Sensors and Actions and connects them
    with the World Model.
3.  Devices - Specific types of devices, e.g. a Sonnen v6 Battery, a Tesla Model X. Knows how to
    talk to the device. Provides any number of Sensors and Actions to the World Model.
4.  Sensor - Reports to the Model what the real world is.
5.  Action - Allows the Model to do something in the real world.

### The World Model

The world model uses object oriented design patterns to keep a digital twin of the real world. As an
explicit choice, the object oriented desing is NOT used to make the system more modular. Instead, we
prioritize robusntess and fail-safe behaviour.

The World Model keeps three states: the state of the world as last observed, the current state of
the world as best estimated and the desired state of the world.

The center of the world is the Home object. The Home is mostly a container for other objects,
although it does directly hold some information like the name.

The Home holds Energy, Security, Communication, Environment and Household objects. Each object holds
the respective model.

As an example, to get the estimated total current energy usage, the following code would be used
`home.energy.usage.total.estimated`. If the latest measured energy usage is needed,
`home.energy.usage.total.measured` can be used. Note however here there is a chance that the return
value is None. This can happen if there are more than one energy measurement device and their last
measure timestamp diverges. In that case, individual measurements can be found using
`home.energy.sensor[0].usage` which will return a (Wh, timestamp) result for the first energy sensor,
or None if the measurement was never recieved.

To prevent the README from diverging from the code, we don't list details of each object here. The
source code is the source of truth.

### Device manager

Scans the networks continously to discover devices and performs health check on configured devices.

### Devices

Individual implementation for specific device _types_. For example, a IKEA smart plug, or a
Gardena gateway. For every actual physical devices, one instance of the class is instantiated.

### Actions

Actions that the model can ask the devices to do.

### Sensors

Measurements the devices give to the model. The model always expects sensors to _push_ data. If a
device protocol is based on pulling the data from the sensor, the device code should
periodically pull the data and push it to the model.

### Who is allowed to talk to whom?

The World Model and the Device Manager are the only objects allowed to talk to the DB.

The device manager instantiates device instances and connects them to the model.

The model calls Actions, and the Senors update the Model.

## Technology

The project is completely written in Rust.

SQLite is used to store user configuration and other runtime data.

tsink is used to store time series data, e.g. the electricity usage time series.

The UI is a Terminal UI written using the excellent Ratatui and Crossterm libraries.

### UI design patterns

The UI uses the [Component architecture](https://ratatui.rs/concepts/application-patterns/component-architecture/).

Each component encapsulates its own state, event handlers, and rendering logic.

## Code quality

Code must always be formatted using the automated standard Rust formatter.

No Rust check errors are allowed. Rust check should be run frequently.

## Testing

Automated tests should be run frequently during coding.

Benchmarks should be used to measure quality. These should be run on demand.

### Automated tests

Each file in src/ should end with the test module for that file, as is typicall in Rust. These tests
should test both happy-path and corner cases.

**Tests in src/ must run in under 1 second**.

Each general user flow (e.g. adding a new directory to be watched for PDFs, removing a directory,
updating a PDF and checking that the txt file updates) should have a test in test/. These should all
be happy-path tests, they should not test errors unless the error is a general user flow.

**Tests in tests/ must run in under 5 seconds.**

### How should tests handle dependencies?

*No mocks*. Mocks prevent testing through the interface and are brittle.

Ideally, the real implementation is used.

When necessary, e.g. for filesystem or database access, fake in-memory implementations should be used.

The src/test/harness.rs test harness should provide convenience functions for faking network
devices and the file system.

## Code structure

Rust's project structure must be followed.

<root of the repository>
    |- /src             <- The implementation
        |- main.rs      <- The main entry point, spawns the background threads and the UI
        |- lib.rs       <- Library exports
        |- app.rs       <- The app controller, responds to events and controls the UI
        |- headless.rs   <- Headless mode entry point
        |- path.rs       <- Shared path utilities for database files
        |- model/       <- The world model (digital twin)
            |- SPECS.md
            |- mod.rs
            |- home.rs
            |- energy.rs
            |- security.rs
            |- environment.rs
            |- communication.rs
            |- household.rs
            |- value.rs
            |- sensor.rs
        |- actions.rs
        |- sensors.rs
        |- devices/     <- The communication layer (talking to devices)
            |- SPECS.md
            |- mod.rs
            |- discovery.rs
            |- protocol.rs
            |- manager.rs
            |- device/   <- Device-specific implementations
                |- device_sonnen.rs
        |- tui/         <- All TUI components go in this directory
            |- SPECS.md <- TUI specs
            |- mod.rs
            |- app.rs
            |- event.rs
            |- theme.rs
            |- components/
                |- mod.rs
                |- gauge.rs
                |- histogram.rs
                |- list.rs
                |- popup.rs
            |- screens/
                |- mod.rs
                |- main_screen.rs
                |- energy_screen.rs
                |- security_screen.rs
                |- environment_screen.rs
                |- communication_screen.rs
                |- household_screen.rs
                |- config_screen.rs
        |- db/          <- The db components and specs
            |- SPECS.md <- Database specs
            |- mod.rs
            |- sqlite.rs
            |- tsink.rs
       |- test/        <- Helper functions for testing
            |- harness.rs
    |- /tests          <- Integration and end-to-end automated tests
        |- gauge_test.rs
        |- network_scanning_test.rs
        |- discovery_test.rs
    |- /benches         <- Benchmarks
    |- README.md        <- This file. Only very high level information goes here
    |- AGENTS.md        <- AI-only instructions
    |- SPECS.md         <- Detailed specifications and all decisions that were taken
    |- TODO.md          <- List of small to  mid size TODO items that need to be fixed in the future
    |- REVIEW.md        <- Comments about the codebase that need to be improved upon

The SPECS.md and README.md files can exist in any subdirectory, and they always serve the same
purpose:

*  README.md - High level summary. Must be readable to humans.
*  SPECS.md - Semi-structured collection of specifications and a decision log of every decision that
   was taken during implementation. SPECS.md files must not contain any code snippets.

The TODO.md and REVIEW.md files are always only in the root of the repository.
