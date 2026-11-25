#include "widget.h"
#include "ui_panel.h"
#include <QLabel>

Widget::Widget(QWidget *parent) : QWidget(parent)
{
    Ui::Panel ui;
    ui.setupUi(this);
}

int Widget::labelCount() const
{
    return findChildren<QLabel *>().size();
}
